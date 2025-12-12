mod api;
pub mod parse;
mod ui;

use std::{
    collections::{BTreeSet, HashMap},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

use clap::CommandFactory;
use indicatif::{ParallelProgressIterator, ProgressBar};
use rayon::{
    iter::{IntoParallelRefIterator, ParallelIterator},
    prelude::IndexedParallelIterator,
};

use crate::{
    cli::parse::{Cli, CompletionShell, ConfigSubcommand, ManifestSubcommand, Subcommand},
    cloud::{CloudChange, Rclone, Remote},
    lang::{Language, TRANSLATOR},
    prelude::{
        app_dir, get_threads_from_env, initialize_rayon, register_sigint, unregister_sigint, Error, Finality,
        StrictPath, SyncDirection,
    },
    report::{self, report_cloud_changes, Reporter},
    resource::{cache::Cache, config::Config, manifest::Manifest, ResourceFile, SaveableResourceFile},
    scan::{
        layout::BackupLayout, prepare_backup_target, scan_game_for_backup, BackupId, DuplicateDetector, Launchers,
        OperationStepDecision, ScanKind, SteamShortcuts, TitleFinder, TitleQuery,
    },
    wrap,
};

const PROGRESS_BAR_REFRESH_INTERVAL: Duration = Duration::from_millis(50);

pub fn show_error(games: &[String], error: &Error, gui: bool, force: bool) {
    let message = TRANSLATOR.handle_error(error);

    if gui {
        let _ = ui::alert(games, gui, force, &message);
    } else {
        eprintln!("{message}");
    }
}

fn negatable_flag(on: bool, off: bool, default: bool) -> bool {
    if on {
        true
    } else if off {
        false
    } else {
        default
    }
}

fn load_manifest(
    config: &Config,
    cache: &mut Cache,
    no_manifest_update: bool,
    try_manifest_update: bool,
) -> Result<Manifest, Error> {
    if no_manifest_update {
        Ok(Manifest::load().unwrap_or_default().with_extensions(config))
    } else if try_manifest_update {
        if let Err(e) = Manifest::update_mut(config, cache, false) {
            eprintln!("{}", TRANSLATOR.handle_error(&e));
        }
        Ok(Manifest::load().unwrap_or_default().with_extensions(config))
    } else {
        Manifest::update_mut(config, cache, false)?;
        Manifest::load().map(|x| x.with_extensions(config))
    }
}

fn parse_game(game: Option<String>) -> Option<String> {
    match game {
        Some(game) => Some(game),
        None => {
            use std::io::IsTerminal;

            let stdin = std::io::stdin();
            if stdin.is_terminal() {
                None
            } else {
                let game = stdin.lines().next().and_then(Result::ok);
                log::debug!("Game from stdin: {:?}", &game);
                game
            }
        }
    }
}

fn parse_games(games: Vec<String>) -> Vec<String> {
    if !games.is_empty() {
        games
    } else {
        use std::io::IsTerminal;

        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            vec![]
        } else {
            let games = stdin.lines().map_while(Result::ok).collect();
            log::debug!("Games from stdin: {:?}", &games);
            games
        }
    }
}

pub fn evaluate_games(
    default: BTreeSet<String>,
    requested: Vec<String>,
    title_finder: &TitleFinder,
) -> Result<Vec<String>, Vec<String>> {
    if requested.is_empty() {
        return Ok(default.into_iter().collect());
    }

    let mut valid = BTreeSet::new();
    let mut invalid = BTreeSet::new();

    for game in requested {
        match title_finder.find_one_by_name(&game) {
            Some(found) => {
                valid.insert(found);
            }
            None => {
                invalid.insert(game);
            }
        }
    }

    if !invalid.is_empty() {
        return Err(invalid.into_iter().collect());
    }

    Ok(valid.into_iter().collect())
}

pub fn parse() -> Result<Cli, clap::Error> {
    use clap::Parser;
    Cli::try_parse()
}

pub fn run(sub: Subcommand, no_manifest_update: bool, try_manifest_update: bool) -> Result<(), Error> {
    let mut config = Config::load()?;
    if let Some(threads) = get_threads_from_env().or(config.runtime.threads) {
        initialize_rayon(threads);
    }
    let mut cache = Cache::load().unwrap_or_default().migrate_config(&mut config);
    TRANSLATOR.set_language(config.language);
    let mut failed = false;
    let mut duplicate_detector = DuplicateDetector::default();

    log::debug!("Config on startup: {config:?}");

    match sub {
        Subcommand::Backup {
            preview,
            gh194,
            path,
            force,
            no_force_cloud_conflict,
            wine_prefix,
            api,
            gui,
            sort,
            format,
            compression,
            compression_level,
            full_limit,
            differential_limit,
            cloud_sync,
            no_cloud_sync,
            dump_registry,
            include_disabled,
            ask_downgrade,
            games,
        } => {
            let games = parse_games(games);

            let mut reporter = if api { Reporter::json() } else { Reporter::standard() };

            let manifest = load_manifest(&config, &mut cache, no_manifest_update, try_manifest_update)?;

            let backup_dir = match path {
                None => config.backup.path.clone(),
                Some(p) => p,
            };
            let roots = config.expanded_roots();

            // FIXME
            if gh194 {
                assert!(preview == true);
            }

            if !ui::confirm(
                &games,
                gui,
                force,
                preview,
                &TRANSLATOR.confirm_backup(&backup_dir, backup_dir.exists(), false),
            )? {
                return Ok(());
            }

            if !preview {
                prepare_backup_target(&backup_dir)?;
            }

            let retention = config.backup.retention.with_limits(full_limit, differential_limit);

            let layout = BackupLayout::new(backup_dir.clone());
            let title_finder = TitleFinder::new(&config, &manifest, layout.restorable_game_set());

            let games_specified = !games.is_empty();
            let games = match evaluate_games(manifest.primary_titles(), games, &title_finder) {
                Ok(games) => games,
                Err(games) => {
                    reporter.trip_unknown_games(games.clone());
                    reporter.print_failure();
                    return Err(Error::CliUnrecognizedGames { games });
                }
            };

            let launchers = Launchers::scan(&roots, &manifest, &games, &title_finder, None);
            let filter = config.backup.filter.clone();
            let toggled_paths = config.backup.toggled_paths.clone();
            let toggled_registry = config.backup.toggled_registry.clone();
            let steam_shortcuts = SteamShortcuts::scan(&title_finder);

            let cloud_sync = negatable_flag(
                cloud_sync && !preview,
                no_cloud_sync,
                config.cloud.synchronize
                    && !preview
                    && crate::cloud::validate_cloud_config(&config, &config.cloud.path).is_ok(),
            );
            let mut should_sync_cloud_after = cloud_sync && !preview;
            let mut should_sync_cloud_after_even_if_unchanged = false;
            if cloud_sync {
                let changes = sync_cloud(
                    &config,
                    &backup_dir,
                    &config.cloud.path,
                    SyncDirection::Upload,
                    Finality::Preview,
                    if games_specified { &games } else { &[] },
                );
                match changes {
                    Ok(changes) => {
                        if !changes.is_empty() {
                            match ui::ask_cloud_conflict(&games, gui, force && !no_force_cloud_conflict, preview)? {
                                Some(direction @ SyncDirection::Download) => {
                                    // We need to download before the new backup
                                    // to keep mapping.yaml in a coherent state.
                                    if let Err(e) = sync_cloud(
                                        &config,
                                        &backup_dir,
                                        &config.cloud.path,
                                        direction,
                                        Finality::Final,
                                        if games_specified { &games } else { &[] },
                                    ) {
                                        log::error!("Failed to resolve save conflict pre-backup with direction {direction:?}: {e:?}");
                                        ui::alert(
                                            &games,
                                            gui,
                                            force && !no_force_cloud_conflict,
                                            &TRANSLATOR.unable_to_synchronize_with_cloud(),
                                        )?;
                                        should_sync_cloud_after = false;
                                        reporter.trip_cloud_sync_failed();
                                    }
                                }
                                Some(SyncDirection::Upload) => {
                                    // We'll make the new backup first and then sync after.
                                    should_sync_cloud_after_even_if_unchanged = true;
                                }
                                None => {
                                    should_sync_cloud_after = false;
                                    reporter.trip_cloud_conflict();
                                }
                            }
                        }
                    }
                    Err(_) => {
                        should_sync_cloud_after = false;
                        reporter.trip_cloud_sync_failed();
                    }
                }
            }

            let step = |i, name| {
                log::trace!("step {i} / {}: {name}", games.len());
                let game = &manifest.0[name];

                let previous = layout.latest_backup(
                    name,
                    ScanKind::Backup,
                    &config.redirects,
                    config.restore.reverse_redirects,
                    &config.restore.toggled_paths,
                    config.backup.only_constructive,
                );

                if filter.excludes(games_specified, previous.is_some(), &game.cloud) {
                    log::trace!("[{name}] excluded by backup filter");
                    return None;
                }

                let scan_info = scan_game_for_backup(
                    game,
                    name,
                    &roots,
                    &app_dir(),
                    &launchers,
                    &filter,
                    wine_prefix.as_ref(),
                    &toggled_paths,
                    &toggled_registry,
                    previous.as_ref(),
                    &config.redirects,
                    config.restore.reverse_redirects,
                    &steam_shortcuts,
                    config.backup.only_constructive,
                );
                let ignored = !&config.is_game_enabled_for_backup(name) && !games_specified && !include_disabled;
                let decision = if ignored {
                    OperationStepDecision::Ignored
                } else {
                    OperationStepDecision::Processed
                };
                let backup_format = || {
                    let mut backup_format = config.backup.format.clone();
                    if let Some(format) = format {
                        backup_format.chosen = format;
                    }
                    if let Some(compression) = compression {
                        backup_format.zip.compression = compression;
                    }
                    if let Some(level) = compression_level {
                        backup_format
                            .compression
                            .set_level(&backup_format.zip.compression, level);
                    }
                    backup_format
                };
                let backup_info = if preview || ignored {
                    None
                } else if ask_downgrade
                    && previous
                        .map(|x| scan_info.is_downgraded_backup(x.when))
                        .unwrap_or_default()
                {
                    match ui::confirm_with_question(
                        &[name.to_string()],
                        gui,
                        false,
                        preview,
                        &TRANSLATOR.backup_is_newer_than_current_data(),
                        &TRANSLATOR.back_up_one_game_confirm(name),
                    ) {
                        Ok(true) => layout.game_layout(name).back_up(
                            &scan_info,
                            &chrono::Utc::now(),
                            &backup_format(),
                            retention,
                            config.backup.only_constructive,
                        ),
                        Ok(false) | Err(_) => None,
                    }
                } else {
                    layout.game_layout(name).back_up(
                        &scan_info,
                        &chrono::Utc::now(),
                        &backup_format(),
                        retention,
                        config.backup.only_constructive,
                    )
                };
                log::trace!("step {i} completed");
                if !scan_info.can_report_game() {
                    None
                } else {
                    let display_title = config.display_name(name);
                    Some((display_title, scan_info, backup_info, decision))
                }
            };

            log::info!("beginning backup with {} steps", games.len());

            let mut info: Vec<_> = if ask_downgrade {
                // Don't parallelize to avoid dialog spam.
                // Don't show progress bar because it blocks the prompt.
                games.iter().enumerate().filter_map(|(i, name)| step(i, name)).collect()
            } else {
                games
                    .par_iter()
                    .enumerate()
                    .progress_with(scan_progress_bar(games.len() as u64))
                    .filter_map(|(i, name)| step(i, name))
                    .collect()
            };
            log::info!("completed backup");

            if gh194 {
                /// Generated by AI (I've pressed tab a few time and I'm actually surprised that it worked this well):
                /// <start of AI>
                ///
                /// For GH-194: Attempt to find mapping between removed and new files based on manifest paths
                /// and scanned paths.
                /// This is a best-effort heuristic and may not be perfect.
                ///
                /// We define a "candidate" as a scanned file that has either been added or removed,
                /// and we try to match removed files to added files based on their manifest paths and
                /// scanned paths.
                ///
                /// We consider a match if the filenames are the same and the base directories
                /// are similar enough (based on common suffix length).
                ///
                /// This is a temporary workaround until a more robust solution is implemented. <-- funny that it describes this as a workaround, what gave it away? :D
                /// We log the mapping in YAML format for easy consumption.
                ///
                /// <end of AI>
                ///
                /// Some of the code is generated with Copilot, mostly the `println!` statements and comments.
                ///
                /// Tested with the following (Steam) games (and more not listed):
                ///  - Factorio (native Linux)
                ///  - Borderlands 2 (native Linux)
                ///  - Alien Breed: Impact (GOG, Heroic)
                ///  - Hoard
                ///  - Distant Worlds 2
                ///  - Dead Island 2 (Epic, Heroic)
                ///
                /// Note: Native Linux games may use an incompatible save file format.
                ///
                /// Current limitations:
                /// - Screenshots and registry are not considered.
                /// - Matching is based on file name. If you have two installations of the same game without any equal file name, they won't be matched
                ///   (happened to me with a Fallout game, for unknown reasons my saves have different names)
                /// - When a game uses different capitalization they are mappable but Ludusavi will still treat the same file as two individual
                ///   (e.g `Borderlands 2` which has a native Linux version)
                /// - Similar to above, when a game uses different paths for what ever reason
                ///   (e.g. `Metro Exodus` uses `Steam3 Hex` on Linux and `AccountID` + `SteamID` on Windows)
                /// - When a game has overlapping entries in the manifest, e.g. `<root>/data` and `<root>/data/saves`, there is no form of prioritization
                ///
                use crate::scan::ScanChange;
                #[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
                struct Candidate {
                    file: String,
                    manifest_path: String,
                    scanned_path: StrictPath,
                }

                /// Calculate the length of the common suffix between two strings, case-insensitively.
                fn common_suffix_length(s1: &str, s2: &str) -> usize {
                    s1.to_lowercase()
                        .chars()
                        .rev() // Start from the end of the first string
                        .zip(s2.to_lowercase().chars().rev()) // Pair it with the end of the second string
                        .take_while(|(c1, c2)| c1 == c2) // Keep taking as long as the characters match
                        .count() // Count how many pairs matched
                }

                /// Find a destination candidate for a given source candidate based on file name and scanned path similarity.
                fn find_dest(
                    destination_candidates: &BTreeSet<Candidate>,
                    file: &str,
                    scanned_path: &StrictPath,
                ) -> Vec<Candidate> {
                    /// FIXME arbitrary threshold
                    /// if it's too low, we get false positives
                    /// if it's too high, we miss some matches (e.g. short paths like `<root>/saves/`)
                    /// 5 seems to be decent enough
                    const THRESHOLD: usize = 5;

                    // In a perfect world, this should always result in exactly one match.
                    // We do not distinguish between `config` and `save` (or anything else) so this can end up with multiple candidates
                    let mut res: Vec<_> = Vec::new();
                    // go through all possible destinations that contain the same file name
                    for candidate in destination_candidates.iter().filter(|c| c.file == file) {
                        // println!(
                        //     "[GH-194] Considering candidate\n  {:?}\n  for file {}, and scanned path {})",
                        //     candidate,
                        //     file,
                        //     scanned_path.raw()
                        // );

                        // The file matches, now check for the rest of the path
                        if common_suffix_length(scanned_path.raw(), candidate.scanned_path.raw())
                            > file.len() + THRESHOLD
                        {
                            // directories match at least `THRESHOLD` characters

                            // check for user id workaround
                            if candidate.manifest_path.contains("<storeUserId>") {
                                // make sure that the user ids are the same
                                // println!(
                                //     "[GH-194]     Manifest path {} contains <storeUserId>, checking user ids",
                                //     candidate.manifest_path
                                // );

                                // very ugly but does the job
                                // we hijack the regex based matching to extract the user ids
                                let ((_, _, user_id_a), (_, _, user_id_b)) = (
                                    match_path_to_manifest(&candidate.manifest_path, scanned_path),
                                    match_path_to_manifest(&candidate.manifest_path, &candidate.scanned_path),
                                );

                                // I would never write so much debug prints myself, thanks Copilot :D
                                if match (user_id_a, user_id_b) {
                                    (Some(a), Some(b)) if a == b => {
                                        // println!(
                                        //     "[GH-194]     Selected candidate manifest path {} with storeUserId {} (matching ids)",
                                        //     candidate.manifest_path, a
                                        // );
                                        true
                                    }
                                    (Some(_a), Some(_b)) => {
                                        // println!(
                                        //     "[GH-194]     Rejected candidate manifest path {} with storeUserId {} (does not match {})",
                                        //     candidate.manifest_path, b, a
                                        // );
                                        false
                                    }
                                    (None, Some(b)) => {
                                        if scanned_path.raw().contains(&b) {
                                            // println!(
                                            //     "[GH-194]     Selected candidate manifest path {} with storeUserId {}",
                                            //     candidate.manifest_path, b
                                            // );
                                            true
                                        } else {
                                            // println!(
                                            //     "[GH-194]     Rejected candidate manifest path {} with storeUserId {}",
                                            //     candidate.manifest_path, b
                                            // );
                                            false
                                        }
                                    }
                                    (Some(a), None) => {
                                        if candidate.scanned_path.raw().contains(&a) {
                                            // println!(
                                            //     "[GH-194]     Selected candidate manifest path {} with storeUserId {}",
                                            //     candidate.manifest_path, a
                                            // );
                                            true
                                        } else {
                                            // println!(
                                            //     "[GH-194]     Rejected candidate manifest path {} with storeUserId {}",
                                            //     candidate.manifest_path, a
                                            // );
                                            false
                                        }
                                    }
                                    (None, None) => {
                                        // println!(
                                        //     "[GH-194]     Unable to determine storeUserId for candidate manifest path {}, skipping",
                                        //     candidate.manifest_path
                                        // );
                                        false
                                    }
                                } {
                                    // println!(
                                    //     "[GH-194]     Selected candidate manifest path {}",
                                    //     candidate.manifest_path
                                    // );

                                    res.push(Candidate {
                                        file: file.to_string(),
                                        manifest_path: (candidate.manifest_path).to_owned(),
                                        scanned_path: (candidate.scanned_path).to_owned(),
                                    });
                                }
                            } else {
                                // simply insert

                                // println!(
                                //     "[GH-194]     Selected candidate manifest path {}",
                                //     candidate.manifest_path
                                // );
                                res.push(Candidate {
                                    file: file.to_string(),
                                    manifest_path: (candidate.manifest_path).to_owned(),
                                    scanned_path: (candidate.scanned_path).to_owned(),
                                });
                            }
                        }
                    }

                    res
                }

                /// For a given template manifest path and expanded scanned path, find the base path that was captured.
                /// Returns: to what the template path was expanded to
                fn find_base_path(template_path_str: &str, expanded_path_str: &str) -> Option<String> {
                    // println!("[GH-194] Searching base for {expanded_path_str} based on {template_path_str}");
                    let res = match_path_to_manifest(template_path_str, &StrictPath::new(expanded_path_str))
                        .1
                        // .and_then(|path| {
                        //     println!("[GH-194]   checking {} vs {:?}", template_path_str, path);
                        //     const THRESHOLD: usize = 5;
                        //     // FIXME is this necessary? EDIT: I don't think so
                        //     if common_suffix_length(template_path_str, &path) < THRESHOLD && false {
                        //         None
                        //     } else {
                        //         Some(path)
                        //     }
                        // })
                        ;
                    // println!("[GH-194]   found {:?}", res);
                    res
                }

                /// Match a scanned path against a manifest path with placeholders.
                /// Returns:
                ///  - true when matched
                ///  - the matched expanded path (i.e. what the template path would expand to - or was)
                ///  - the `user_id` when found (see `<storeUserId>`)
                fn match_path_to_manifest(
                    manifest_path_in: &str,
                    scanned_path: &StrictPath,
                ) -> (bool, Option<String>, Option<String>) {
                    // basic building blocks for path regexes
                    const PATH_ELEM: &str = r"(?:/[\w\s\.\-]+)"; // good enough? or just go with `/.+`? FYI https://stackoverflow.com/a/24703223
                    const WIN_DRIVE: &str = r"(?:[C-Z]:)"; // is someone using A or B drive these days?

                    // Windows root can either be a drive letter or a wine prefix
                    let win_drive_wine_prefix = format!("(?:{PATH_ELEM}+/pfx/drive_[c-z])");

                    let path_with_root_any = format!("^{WIN_DRIVE}?{PATH_ELEM}+");
                    let path_with_root_lin = format!("^{PATH_ELEM}+");
                    let path_with_root_win = format!("^(?:{WIN_DRIVE}|{win_drive_wine_prefix}){PATH_ELEM}+");

                    // These old Windows XP path used by wine are only partly tested (uncovered with `Red Matter`)
                    let path_with_root_appdata =
                        format!("{path_with_root_win}{PATH_ELEM}+/(?:AppData/Roaming|Application Data)");
                    let path_with_root_appdata_local =
                        format!("{path_with_root_win}{PATH_ELEM}+/(?:AppData/Local|Local Settings/Application Data)");
                    let path_with_root_appdata_low = format!(
                        "{path_with_root_win}{PATH_ELEM}+/(?:AppData/LocalLow|Local Settings/Application Data)"
                    );
                    // not found in manifest at the moment
                    // let game = format!("{PATH_ELEM}+"); // is this good enough? Documentation says `game` can be just a name as well

                    // replace placeholder by matching regex
                    let manifest_path = String::from("(?i)") // turn on case insensitivity due to mixture of cases
                        + PathBuf::from(manifest_path_in)
                            .components()
                            .map(|c| c.as_os_str().to_string_lossy().to_string())
                            .map(|component| {
                                if component.starts_with('<') && component.ends_with('>') {
                                    match component.as_ref() {
                                        "<root>" => path_with_root_any.as_ref(),
                                        // "<game>" => game.as_ref(), see above
                                        "<base>" => path_with_root_any.as_ref(),
                                        "<home>" => path_with_root_any.as_ref(),
                                        "<storeGameId>" => r"(?:\d+)",
                                        "<storeUserId>" => r"(?<user_id>\d+)",
                                        // "<osUserName>" => "current user's name in the OS",
                                        "<winAppData>" =>  path_with_root_appdata.as_ref(),
                                        "<winLocalAppData>" => path_with_root_appdata_local.as_ref(),
                                        "<winLocalAppDataLow>" => path_with_root_appdata_low.as_ref(),

                                        // This could include some localized `Documents` folder name
                                        // This can also be used on other OSs than Windows
                                        "<winDocuments>" => path_with_root_any.as_ref(),

                                        // "<winPublic>" => "%PUBLIC% on Windows",
                                        // "<winProgramData>" => "%PROGRAMDATA% on Windows",
                                        // "<winDir>" => "%WINDIR% on Windows",
                                        "<xdgData>" => path_with_root_lin.as_ref(),
                                        "<xdgConfig>" => path_with_root_lin.as_ref(),
                                        placeholder => unimplemented!("Unknown placeholder: {}", placeholder),
                                    }
                                    .to_string()
                                } else {
                                    regex::escape(&component)
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("/")
                            .as_ref();

                    // println!("[GH-194] mapped {manifest_path_in} to {manifest_path}");

                    let re = regex::Regex::new(&manifest_path).unwrap();
                    match re.captures(scanned_path.raw()) {
                        Some(caps) => (
                            true,
                            caps.get(0).map(|cap| cap.as_str().to_string()),
                            // ugly hack to access the user_id in `find_dest`
                            caps.name("user_id").map(|cap| cap.as_str().to_string()),
                        ),
                        None => (false, None, None),
                    }
                }

                /// For a given manifest path, find all matching scanned paths and categorize them as source or destination
                fn match_scanned_path(
                    manifest_path: &String,
                    scan_info: &crate::scan::ScanInfo,
                    destination: &mut BTreeSet<Candidate>,
                    source: &mut BTreeSet<Candidate>,
                    mapping: &mut HashMap<String, BTreeSet<(String, String)>>,
                ) {
                    for (scanned_path, scanned_info) in &scan_info.found_files {
                        if match_path_to_manifest(manifest_path, scanned_path).0 {
                            // println!(
                            //     "[GH-194] Matched manifest path {} to scanned path {} with action {:?}",
                            //     manifest_path,
                            //     scanned_path.raw(),
                            //     scanned_info.change(),
                            // );
                        } else {
                            // println!(
                            //     "[GH-194] Did not match manifest path {} to scanned path {}",
                            //     manifest_path,
                            //     scanned_path.raw()
                            // );
                            continue;
                        }

                        let file = PathBuf::from(scanned_path.raw())
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .to_lowercase(); // keep everything lower case; FIXME different capitalization is a known problem

                        match scanned_info.change() {
                            ScanChange::New => {
                                // We are looking for a source for this path
                                destination.insert(Candidate {
                                    file: file.to_string(),
                                    manifest_path: manifest_path.to_owned(),
                                    scanned_path: scanned_path.clone(),
                                });
                            }
                            ScanChange::Removed => {
                                // We are looking for a destination for this path
                                source.insert(Candidate {
                                    file: file.to_string(),
                                    manifest_path: manifest_path.to_owned(),
                                    scanned_path: scanned_path.clone(),
                                });
                            }
                            _ => continue,
                        }

                        if !mapping.contains_key(manifest_path) {
                            mapping.insert(manifest_path.to_owned(), BTreeSet::new());
                        }
                    }
                }

                for (game_name, scan_info, _backup_info, _decision) in info.iter() {
                    // Summarize changes
                    let (new, remove, _other) =
                        scan_info
                            .found_files
                            .values()
                            .fold((0, 0, 0), |(mut new, mut remove, mut other), file| {
                                match file.change {
                                    crate::scan::ScanChange::New => new += 1,
                                    crate::scan::ScanChange::Removed => remove += 1,
                                    _ => other += 1,
                                }
                                (new, remove, other)
                            });
                    // println!("[GH-194] Summary: new={}, removed={}, other={}", new, remove, other);

                    // Calculate "mapping" probability
                    let _sum = scan_info.found_files.len() as i32;
                    // at minimum we need a diff where files are removed AND added
                    let do_mapping = new > 0 && remove > 0; // FIXME eventually use a better heuristic
                    if !do_mapping {
                        continue;
                    }

                    let game_info = manifest.0.get(&game_name.to_string()).unwrap();
                    // println!("[GH-194] info: {:#?}", game_info);

                    let mut mapping: HashMap<String, BTreeSet<(String, String)>> = HashMap::new();
                    let mut source_candidates: BTreeSet<Candidate> = BTreeSet::new();
                    let mut destination_candidates: BTreeSet<Candidate> = BTreeSet::new();

                    // Step 1: go through all manifest listed paths and search for matching changes
                    for (manifest_path, _) in game_info.files.iter() {
                        // check if path is used by modified files
                        match_scanned_path(
                            manifest_path,
                            scan_info,
                            &mut destination_candidates,
                            &mut source_candidates,
                            &mut mapping,
                        );
                    }

                    // Step 1.1: handle special cases like Steam
                    let all_ids = game_info.all_ids();
                    let steam_shortcut = steam_shortcuts.get(&scan_info.game_name);
                    for id in all_ids.steam(steam_shortcut.map(|x| x.id)) {
                        // from src/scan.rs::scan_game_for_backup()
                        // Cloud saves:
                        // paths_to_check.insert((
                        //     StrictPath::relative(
                        //         format!("{}/userdata/*/{}/remote/", &root_globbable, id),
                        //         Some(manifest_dir_globbable.clone()),
                        //     ),
                        //     None,
                        // ));

                        let manifest_path = format!("<root>/userdata/<storeUserId>/{}/remote/", id);

                        // This approach results in `double adds` like
                        //    "<root>/userdata/<storeUserId>/63000": {},
                        //    "<root>/userdata/<storeUserId>/63000/remote/": {},
                        // Use this simple hack to avoid adding a path if a short version is already present
                        {
                            // Initial idea to create the expected short version
                            // let tmp = format!("<root>/userdata/<storeUserId>/{}", id); // no trailing `/`
                            // if mapping.contains_key(&tmp) {
                            //     continue;
                            // }

                            // But this is better as it should catch all cases of shorter paths (no idea if there are cases that the above one would not catch)
                            if mapping.keys().find(|&key| manifest_path.contains(key)).is_some() {
                                continue;
                            }
                        }

                        // println!(
                        //     "[GH-194] Checking manifest path {} for Steam cloud saves",
                        //     manifest_path
                        // );
                        match_scanned_path(
                            &manifest_path,
                            scan_info,
                            &mut destination_candidates,
                            &mut source_candidates,
                            &mut mapping,
                        );
                    }

                    // Step 2: go through all manifest paths and check if we have a match for them
                    // println!("[GH-194] Mapping {:#?}", mapping);
                    mapping.iter_mut().for_each(|(manifest_path, entry)| {
                        // assert `is_empty`?
                        assert!(entry.is_empty());

                        // go through all sources for the given manifest entry
                        for source in source_candidates.iter().filter(|c| &c.manifest_path == manifest_path) {
                            // for each source, try to find a destination
                            let candidates = find_dest(&destination_candidates, &source.file, &source.scanned_path);

                            // println!("[GH-194]   Candidates {:#?}", candidates);
                            for c in candidates {
                                // println!(
                                //     "[GH-194]   Searching for mapping {:#?} <-> {:?}",
                                //     manifest_path, c.manifest_path
                                // );

                                let src = find_base_path(&manifest_path, source.scanned_path.raw())
                                    .expect("This should not happen, right?");

                                let dst = find_base_path(&c.manifest_path, c.scanned_path.raw())
                                    .expect("This should not happen, right?");

                                // I have no idea how we can end up with different path here.
                                // One theory is related to Windows <-> Linux mappings with similar paths in the manifest.
                                // Another one is that some matching code somewhere is broken (likely?)
                                // This was uncovered by testing `Stardew Valley`, also fixes `Hoard`
                                if common_suffix_length(&src, &dst) < 5 {
                                    // println!("[GH-194] DROPPING none matching pair: {src} => {dst}");
                                    continue;
                                }
                                entry.insert((src, dst));
                            }
                        }
                    });

                    // println!("[GH-194] Source Candidates {:#?}", source_candidates);
                    // println!("[GH-194] Destination Candidates {:#?}", destination_candidates);
                    // println!("[GH-194] Mapping {:#?}", mapping);

                    // Output mapping
                    // for (_source, dest) in mapping.iter() {
                    //     if dest.is_empty() {
                    //         // TODO should we do something about unmapped paths?
                    //         // I haven't encountered any issue so far.
                    //         // On the contrary this happens easily when a game has multiple similar path, e.g. `Stadew Valley`:
                    //         // [GH-194] MAPPING: <home>/.config/StardewValley => <unmapped>
                    //         // [GH-194] MAPPING: <home>/.config/StardewValley/Saves => <unmapped>
                    //         // [GH-194] MAPPING: <xdgConfig>/StardewValley => <unmapped>
                    //         // [GH-194] MAPPING: <xdgConfig>/StardewValley/Saves => <unmapped>
                    //         // all end with `/StardewValley[/Saves]`

                    //         // println!("[GH-194] MAPPING: {} => <unmapped>", source);
                    //         continue;
                    //     }

                    //     println!("[GH-194] MAPPING: {} => {:?}", source, dest);
                    // }
                    for (_source, dest_vec) in mapping.iter().filter(|e| !e.1.is_empty()) {
                        for dest in dest_vec {
                            // Output in YAML format, can directly be copied into `config.yaml`
                            //   - kind: bidirectional
                            //     source: /src/a
                            //     target: /dst/b
                            println!(
                                "  - kind: bidirectional\n    source: {}\n    target: {}",
                                dest.1, dest.0
                            );
                        }
                    }
                }
            }

            if should_sync_cloud_after {
                let changed_games: Vec<_> = info
                    .iter()
                    .filter(|(_, scan_info, backup_info, _)| scan_info.needs_cloud_sync() && backup_info.is_some())
                    .map(|(_, scan_info, _, _)| scan_info.game_name.clone())
                    .collect();
                if !changed_games.is_empty() || should_sync_cloud_after_even_if_unchanged {
                    let sync_result = sync_cloud(
                        &config,
                        &backup_dir,
                        &config.cloud.path,
                        SyncDirection::Upload,
                        Finality::Final,
                        &changed_games,
                    );
                    if sync_result.is_err() {
                        reporter.trip_cloud_sync_failed();
                    }
                }
            }

            for (_, scan_info, _, _) in info.iter() {
                duplicate_detector.add_game(
                    scan_info,
                    config.is_game_enabled_for_operation(&scan_info.game_name, ScanKind::Backup),
                );
            }

            let sort = sort.map(From::from).unwrap_or_else(|| config.backup.sort.clone());
            info.sort_by(
                |(name1, scan_info1, backup_info1, ..), (name2, scan_info2, backup_info2, ..)| {
                    crate::scan::compare_games(
                        sort.key,
                        &config,
                        name1,
                        scan_info1,
                        backup_info1.as_ref(),
                        name2,
                        scan_info2,
                        backup_info2.as_ref(),
                    )
                },
            );
            if sort.reversed {
                info.reverse();
            }

            for (name, scan_info, backup_info, decision) in info {
                if !reporter.add_game(
                    name,
                    &scan_info,
                    backup_info.as_ref(),
                    &decision,
                    &duplicate_detector,
                    dump_registry,
                ) {
                    failed = true;
                }
            }
            reporter.print(&backup_dir);
        }
        Subcommand::Restore {
            preview,
            path,
            force,
            no_force_cloud_conflict,
            api,
            gui,
            sort,
            backup,
            cloud_sync,
            no_cloud_sync,
            dump_registry,
            include_disabled,
            ask_downgrade,
            games,
        } => {
            let games = parse_games(games);

            let mut reporter = if api { Reporter::json() } else { Reporter::standard() };

            let restore_dir = match path {
                None => config.restore.path.clone(),
                Some(p) => p,
            };

            if !ui::confirm(
                &games,
                gui,
                force,
                preview,
                &TRANSLATOR.confirm_restore(&restore_dir, false),
            )? {
                return Ok(());
            }

            let layout = BackupLayout::new(restore_dir.clone());

            if backup.is_some() && games.len() != 1 {
                return Err(Error::CliBackupIdWithMultipleGames);
            }
            let backup_id = backup.as_ref().map(|x| BackupId::Named(x.clone()));

            let manifest = load_manifest(&config, &mut cache, true, false).unwrap_or_default();
            let title_finder = TitleFinder::new(&config, &manifest, layout.restorable_game_set());

            let games_specified = !games.is_empty();
            let games = match evaluate_games(layout.restorable_game_set(), games, &title_finder) {
                Ok(games) => games,
                Err(games) => {
                    reporter.trip_unknown_games(games.clone());
                    reporter.print_failure();
                    return Err(Error::CliUnrecognizedGames { games });
                }
            };

            let cloud_sync = negatable_flag(
                cloud_sync && !preview,
                no_cloud_sync,
                config.cloud.synchronize
                    && !preview
                    && crate::cloud::validate_cloud_config(&config, &config.cloud.path).is_ok(),
            );
            if cloud_sync {
                let changes = sync_cloud(
                    &config,
                    &restore_dir,
                    &config.cloud.path,
                    SyncDirection::Upload,
                    Finality::Preview,
                    if games_specified { &games } else { &[] },
                );
                match changes {
                    Ok(changes) => {
                        if !changes.is_empty() {
                            match ui::ask_cloud_conflict(&games, gui, force && !no_force_cloud_conflict, preview)? {
                                Some(direction) => {
                                    if let Err(e) = sync_cloud(
                                        &config,
                                        &restore_dir,
                                        &config.cloud.path,
                                        direction,
                                        Finality::Final,
                                        if games_specified { &games } else { &[] },
                                    ) {
                                        log::error!("Failed to resolve save conflict pre-restore with direction {direction:?}: {e:?}");
                                        ui::alert(
                                            &games,
                                            gui,
                                            force && !no_force_cloud_conflict,
                                            &TRANSLATOR.unable_to_synchronize_with_cloud(),
                                        )?;
                                        reporter.trip_cloud_sync_failed();
                                    }
                                }
                                None => {
                                    reporter.trip_cloud_conflict();
                                }
                            }
                        }
                    }
                    Err(_) => {
                        reporter.trip_cloud_sync_failed();
                    }
                }
            }

            let step = |i, name| {
                log::trace!("step {i} / {}: {name}", games.len());
                let mut layout = layout.game_layout(name);
                let scan_info = layout.scan_for_restoration(
                    name,
                    backup_id.as_ref().unwrap_or(&BackupId::Latest),
                    &config.redirects,
                    config.restore.reverse_redirects,
                    &config.restore.toggled_paths,
                    &config.restore.toggled_registry,
                );
                let ignored = !&config.is_game_enabled_for_restore(name) && !games_specified && !include_disabled;
                let decision = if ignored {
                    OperationStepDecision::Ignored
                } else {
                    OperationStepDecision::Processed
                };

                if let Some(backup) = &backup {
                    if let Some(BackupId::Named(scanned_backup)) = scan_info.backup.as_ref().map(|x| x.id()) {
                        if backup != &scanned_backup {
                            log::trace!("step {i} completed (backup mismatch)");
                            let display_title = config.display_name(name);
                            return Some((
                                display_title,
                                scan_info,
                                Default::default(),
                                decision,
                                Some(Err(Error::CliInvalidBackupId)),
                            ));
                        }
                    }
                }

                let restore_info = if scan_info.backup.is_none() || preview || ignored {
                    None
                } else if ask_downgrade && scan_info.is_downgraded_restore() {
                    match ui::confirm_with_question(
                        &[name.to_string()],
                        gui,
                        false,
                        preview,
                        &TRANSLATOR.backup_is_older_than_current_data(),
                        &TRANSLATOR.restore_one_game_confirm(name),
                    ) {
                        Ok(true) => Some(layout.restore(&scan_info, &config.restore.toggled_registry)),
                        Ok(false) | Err(_) => None,
                    }
                } else {
                    Some(layout.restore(&scan_info, &config.restore.toggled_registry))
                };
                log::trace!("step {i} completed");
                if !scan_info.can_report_game() {
                    None
                } else {
                    let display_title = config.display_name(name);
                    Some((display_title, scan_info, restore_info, decision, None))
                }
            };

            log::info!("beginning restore with {} steps", games.len());

            let mut info: Vec<_> = if ask_downgrade {
                // Don't parallelize to avoid dialog spam.
                // Don't show progress bar because it blocks the prompt.
                games.iter().enumerate().filter_map(|(i, name)| step(i, name)).collect()
            } else {
                games
                    .par_iter()
                    .enumerate()
                    .progress_with(scan_progress_bar(games.len() as u64))
                    .filter_map(|(i, name)| step(i, name))
                    .collect()
            };
            log::info!("completed restore");

            for (_, scan_info, _, _, failure) in info.iter() {
                if let Some(failure) = failure {
                    return failure.clone();
                }
                duplicate_detector.add_game(
                    scan_info,
                    config.is_game_enabled_for_operation(&scan_info.game_name, ScanKind::Restore),
                );
            }

            let sort = sort.map(From::from).unwrap_or_else(|| config.restore.sort.clone());
            info.sort_by(
                |(name1, scan_info1, backup_info1, ..), (name2, scan_info2, backup_info2, ..)| {
                    crate::scan::compare_games(
                        sort.key,
                        &config,
                        name1,
                        scan_info1,
                        backup_info1.as_ref(),
                        name2,
                        scan_info2,
                        backup_info2.as_ref(),
                    )
                },
            );
            if sort.reversed {
                info.reverse();
            }

            for (name, scan_info, backup_info, decision, _) in info {
                if !reporter.add_game(
                    name,
                    &scan_info,
                    backup_info.as_ref(),
                    &decision,
                    &duplicate_detector,
                    dump_registry,
                ) {
                    failed = true;
                }
            }
            reporter.print(&restore_dir);
        }
        Subcommand::Complete { shell } => {
            let clap_shell = match shell {
                CompletionShell::Bash => clap_complete::Shell::Bash,
                CompletionShell::Fish => clap_complete::Shell::Fish,
                CompletionShell::Zsh => clap_complete::Shell::Zsh,
                CompletionShell::PowerShell => clap_complete::Shell::PowerShell,
                CompletionShell::Elvish => clap_complete::Shell::Elvish,
            };
            clap_complete::generate(
                clap_shell,
                &mut Cli::command(),
                env!("CARGO_PKG_NAME"),
                &mut std::io::stdout(),
            )
        }
        Subcommand::Backups { sub, path, api, games } => match sub {
            Some(parse::BackupsSubcommand::Edit {
                path,
                backup,
                lock,
                unlock,
                comment,
                game,
            }) => {
                let backup = backup.map(BackupId::Named).unwrap_or(BackupId::Latest);

                let Some(game) = parse_game(game) else {
                    return Err(Error::GameIsUnrecognized);
                };

                let restore_dir = match path {
                    None => config.restore.path.clone(),
                    Some(p) => p,
                };

                let layout = BackupLayout::new(restore_dir.clone());
                let manifest = load_manifest(&config, &mut cache, true, false).unwrap_or_default();
                let title_finder = TitleFinder::new(&config, &manifest, layout.restorable_game_set());

                let Some(game) = title_finder.find_one_by_name(&game) else {
                    return Err(Error::GameIsUnrecognized);
                };

                let mut layout = layout.game_layout(&game);
                layout.validate_id(&backup)?;

                if lock {
                    layout.set_backup_locked(&backup, true);
                }
                if unlock {
                    layout.set_backup_locked(&backup, false);
                }
                if let Some(comment) = comment {
                    layout.set_backup_comment(&backup, &comment);
                }
                layout.save();

                return Ok(());
            }
            None => {
                let games = parse_games(games);

                let mut reporter = if api { Reporter::json() } else { Reporter::standard() };
                reporter.suppress_overall();

                let restore_dir = match path {
                    None => config.restore.path.clone(),
                    Some(p) => p,
                };

                let layout = BackupLayout::new(restore_dir.clone());
                let manifest = load_manifest(&config, &mut cache, true, false).unwrap_or_default();
                let title_finder = TitleFinder::new(&config, &manifest, layout.restorable_game_set());

                let games = match evaluate_games(layout.restorable_game_set(), games, &title_finder) {
                    Ok(games) => games,
                    Err(games) => {
                        reporter.trip_unknown_games(games.clone());
                        reporter.print_failure();
                        return Err(Error::CliUnrecognizedGames { games });
                    }
                };

                let info: Vec<_> = games
                    .par_iter()
                    .progress_count(games.len() as u64)
                    .map(|name| {
                        let mut layout = layout.game_layout(name);
                        let backups = layout.get_backups();
                        let display_title = config.display_name(name);
                        let backup_dir = layout.path;
                        (name, display_title, backup_dir, backups)
                    })
                    .collect();

                for (name, display_title, backup_dir, backups) in info {
                    reporter.add_backups(name, display_title, backup_dir, &backups);
                }
                reporter.print(&restore_dir);
            }
        },
        Subcommand::Find {
            api,
            multiple,
            path,
            backup,
            restore,
            steam_id,
            gog_id,
            lutris_id,
            normalized,
            fuzzy,
            disabled,
            partial,
            names,
        } => {
            let names = parse_games(names);

            let mut reporter = if api { Reporter::json() } else { Reporter::standard() };
            reporter.suppress_overall();

            let manifest = load_manifest(&config, &mut cache, no_manifest_update, try_manifest_update)?;

            let restore_dir = match path {
                None => config.restore.path.clone(),
                Some(p) => p,
            };
            let layout = BackupLayout::new(restore_dir.clone());

            let title_finder = TitleFinder::new(&config, &manifest, layout.restorable_game_set());
            let found = title_finder.find(TitleQuery {
                multiple,
                names: names.clone(),
                steam_id,
                gog_id,
                lutris_id,
                normalized,
                fuzzy,
                backup,
                restore,
                disabled,
                partial,
            });
            reporter.add_found_titles(&found);

            if found.is_empty() {
                let mut invalid = names;
                if let Some(steam_id) = steam_id {
                    invalid.push(steam_id.to_string());
                }
                if let Some(gog_id) = gog_id {
                    invalid.push(gog_id.to_string());
                }
                reporter.trip_unknown_games(invalid.clone());
                reporter.print_failure();
                return Err(Error::CliUnrecognizedGames { games: invalid });
            }

            reporter.print(&restore_dir);
        }
        Subcommand::Manifest { sub: manifest_sub } => match manifest_sub {
            ManifestSubcommand::Show { api } => {
                let manifest = load_manifest(&config, &mut cache, true, false).unwrap_or_default();

                if api {
                    println!("{}", serde_json::to_string(&manifest).unwrap());
                } else {
                    println!("{}", serde_yaml::to_string(&manifest).unwrap());
                }
            }
            ManifestSubcommand::Update { force } => {
                Manifest::update_mut(&config, &mut cache, force)?;
            }
        },
        Subcommand::Config { sub: config_sub } => match config_sub {
            ConfigSubcommand::Path => {
                println!("{}", Config::path().render());
            }
            ConfigSubcommand::Show { api, default } => {
                if default {
                    config = Config::default();
                }

                if api {
                    println!("{}", serde_json::to_string(&config).unwrap());
                } else {
                    println!("{}", serde_yaml::to_string(&config).unwrap());
                }
            }
        },
        Subcommand::Cloud { sub: cloud_sub } => match cloud_sub {
            parse::CloudSubcommand::Set { sub } => match sub {
                parse::CloudSetSubcommand::None => {
                    config.cloud.remote = None;
                    config.save();
                }
                parse::CloudSetSubcommand::Custom { id } => {
                    configure_cloud(&mut config, Remote::Custom { id })?;
                }
                parse::CloudSetSubcommand::Box => {
                    configure_cloud(
                        &mut config,
                        Remote::Box {
                            id: Remote::generate_id(),
                        },
                    )?;
                }
                parse::CloudSetSubcommand::Dropbox => {
                    configure_cloud(
                        &mut config,
                        Remote::Dropbox {
                            id: Remote::generate_id(),
                        },
                    )?;
                }
                parse::CloudSetSubcommand::Ftp {
                    host,
                    port,
                    username,
                    password,
                } => {
                    configure_cloud(
                        &mut config,
                        Remote::Ftp {
                            id: Remote::generate_id(),
                            host,
                            port,
                            username,
                            password,
                        },
                    )?;
                }
                parse::CloudSetSubcommand::GoogleDrive => {
                    configure_cloud(
                        &mut config,
                        Remote::GoogleDrive {
                            id: Remote::generate_id(),
                        },
                    )?;
                }
                parse::CloudSetSubcommand::OneDrive => {
                    configure_cloud(
                        &mut config,
                        Remote::OneDrive {
                            id: Remote::generate_id(),
                        },
                    )?;
                }
                parse::CloudSetSubcommand::Smb {
                    host,
                    port,
                    username,
                    password,
                } => {
                    configure_cloud(
                        &mut config,
                        Remote::Smb {
                            id: Remote::generate_id(),
                            host,
                            port,
                            username,
                            password,
                        },
                    )?;
                }
                parse::CloudSetSubcommand::WebDav {
                    url,
                    username,
                    password,
                    provider,
                } => {
                    configure_cloud(
                        &mut config,
                        Remote::WebDav {
                            id: Remote::generate_id(),
                            url,
                            username,
                            password,
                            provider,
                        },
                    )?;
                }
            },
            parse::CloudSubcommand::Upload {
                local,
                cloud,
                force,
                preview,
                api,
                gui,
                games,
            } => {
                let games = parse_games(games);

                let local = local.unwrap_or(config.backup.path.clone());
                let cloud = cloud.unwrap_or(config.cloud.path.clone());

                let finality = if preview { Finality::Preview } else { Finality::Final };
                let direction = SyncDirection::Upload;

                let layout = BackupLayout::new(config.restore.path.clone());
                let manifest = load_manifest(&config, &mut cache, true, false).unwrap_or_default();
                let title_finder = TitleFinder::new(&config, &manifest, layout.restorable_game_set());

                let games = match evaluate_games(layout.restorable_game_set(), games, &title_finder) {
                    Ok(games) => games,
                    Err(games) => {
                        let mut reporter = if api { Reporter::json() } else { Reporter::standard() };
                        reporter.trip_unknown_games(games.clone());
                        reporter.print_failure();
                        return Err(Error::CliUnrecognizedGames { games });
                    }
                };

                if !ui::confirm(
                    &games,
                    gui,
                    force,
                    finality.preview(),
                    &TRANSLATOR.confirm_cloud_upload(&local.render(), &cloud),
                )? {
                    return Ok(());
                }

                let changes = sync_cloud(&config, &local, &cloud, direction, finality, &games)?;
                report_cloud_changes(&changes, api);
            }
            parse::CloudSubcommand::Download {
                local,
                cloud,
                force,
                preview,
                api,
                gui,
                games,
            } => {
                let games = parse_games(games);

                let local = local.unwrap_or(config.backup.path.clone());
                let cloud = cloud.unwrap_or(config.cloud.path.clone());

                let finality = if preview { Finality::Preview } else { Finality::Final };
                let direction = SyncDirection::Download;

                let layout = BackupLayout::new(config.restore.path.clone());
                let manifest = load_manifest(&config, &mut cache, true, false).unwrap_or_default();
                let title_finder = TitleFinder::new(&config, &manifest, layout.restorable_game_set());

                let games = match evaluate_games(layout.restorable_game_set(), games, &title_finder) {
                    Ok(games) => games,
                    Err(games) => {
                        let mut reporter = if api { Reporter::json() } else { Reporter::standard() };
                        reporter.trip_unknown_games(games.clone());
                        reporter.print_failure();
                        return Err(Error::CliUnrecognizedGames { games });
                    }
                };

                if !ui::confirm(
                    &games,
                    gui,
                    force,
                    finality.preview(),
                    &TRANSLATOR.confirm_cloud_download(&local.render(), &cloud),
                )? {
                    return Ok(());
                }

                let changes = sync_cloud(&config, &local, &cloud, direction, finality, &games)?;
                report_cloud_changes(&changes, api);
            }
        },
        Subcommand::Wrap {
            name_source,
            force,
            force_backup,
            force_restore,
            no_force_cloud_conflict,
            gui,
            path,
            format,
            compression,
            compression_level,
            full_limit,
            differential_limit,
            cloud_sync,
            no_cloud_sync,
            ask_downgrade,
            commands,
        } => {
            let manifest = load_manifest(&config, &mut cache, no_manifest_update, try_manifest_update)?;
            let layout = BackupLayout::new(config.restore.path.clone());
            let title_finder = TitleFinder::new(&config, &manifest, layout.restorable_game_set());
            let preview = false;

            let force_backup = force || force_backup;
            let force_restore = force || force_restore;

            // Determine raw game identifiers
            let wrap_game_info = if let Some(name) = name_source.name.as_ref() {
                Some(wrap::WrapGameInfo {
                    name: Some(name.clone()),
                    ..Default::default()
                })
            } else if let Some(infer) = name_source.infer {
                let roots = config.expanded_roots();
                match infer {
                    parse::Launcher::Heroic => wrap::heroic::infer_game_from_heroic(&roots),
                    parse::Launcher::Lutris => wrap::lutris::infer(),
                    parse::Launcher::Steam => wrap::infer_game_from_steam(),
                }
            } else {
                unreachable!();
            };
            log::debug!("Wrap game info: {:?}", &wrap_game_info);

            // Check game identifiers against the manifest
            //
            // e.g. "Slain: Back From Hell" from legendary to "Slain: Back from
            // Hell" as known to ludusavi
            let game_name = wrap_game_info.clone().and_then(|info| {
                let names = info.name.map(|x| vec![x]).unwrap_or_default();
                title_finder.find_one(TitleQuery {
                    names,
                    steam_id: info.steam_id,
                    gog_id: info.gog_id,
                    normalized: true,
                    ..Default::default()
                })
            });
            let games = game_name.as_ref().map(|game| vec![game.clone()]).unwrap_or_default();
            log::debug!("Title finder result: {:?}", &game_name);

            match game_name.as_ref() {
                Some(game_name) => {
                    wrap::lutris::save_normalized_title(game_name.clone());
                }
                None => {
                    if !ui::confirm_with_question(
                        &games,
                        gui,
                        force,
                        preview,
                        &TRANSLATOR.game_is_unrecognized(),
                        &TRANSLATOR.launch_game_after_error(),
                    )? {
                        return Ok(());
                    }
                }
            }

            // Restore
            // TODO: Detect if saves are unchanged and skip the question?
            'restore: {
                let Some(game_name) = game_name.as_ref() else {
                    break 'restore;
                };

                let game_layout = layout.game_layout(game_name);
                if !game_layout.has_backups() {
                    if ui::confirm_with_question(
                        &games,
                        gui,
                        force_restore,
                        preview,
                        &TRANSLATOR.game_has_nothing_to_restore(),
                        &TRANSLATOR.launch_game_after_error(),
                    )? {
                        break 'restore;
                    } else {
                        return Ok(());
                    }
                }

                if !ui::confirm(
                    &games,
                    gui,
                    force_restore,
                    preview,
                    &TRANSLATOR.restore_one_game_confirm(game_name),
                )? {
                    break 'restore;
                }

                if let Err(err) = run(
                    Subcommand::Restore {
                        games: vec![game_name.clone()],
                        force: true,
                        no_force_cloud_conflict: no_force_cloud_conflict || !force,
                        preview,
                        path: path.clone(),
                        api: Default::default(),
                        gui,
                        sort: Default::default(),
                        backup: Default::default(),
                        cloud_sync,
                        no_cloud_sync,
                        dump_registry: Default::default(),
                        include_disabled: Default::default(),
                        ask_downgrade,
                    },
                    no_manifest_update,
                    try_manifest_update,
                ) {
                    log::error!("Wrap failed to restore game {:?} with: {:?}", wrap_game_info, err);
                    ui::alert_with_error(
                        &games,
                        gui,
                        force_restore,
                        &TRANSLATOR.restore_one_game_failed(game_name),
                        &err,
                    )?;
                    return Err(err);
                }
            }

            // Launch game
            // TODO: Legendary returns immediately. Can we handle that?
            let result = Command::new(&commands[0]).args(&commands[1..]).status();
            match result {
                Ok(status) => {
                    log::debug!("Wrapped game command executed, returning status: {:#?}", status);
                }
                Err(err) => {
                    log::error!("Wrapped game command execution failed with: {:#?}", err);
                    ui::alert_with_raw_error(&games, gui, force, &TRANSLATOR.game_did_not_launch(), &err.to_string())?;
                    return Err(Error::GameDidNotLaunch { why: err.to_string() });
                }
            }

            // Backup
            'backup: {
                let Some(game_name) = game_name.as_ref() else {
                    break 'backup;
                };

                if !ui::confirm(
                    &games,
                    gui,
                    force_backup,
                    preview,
                    &TRANSLATOR.back_up_one_game_confirm(game_name),
                )? {
                    break 'backup;
                }

                if let Err(err) = run(
                    Subcommand::Backup {
                        games: vec![game_name.clone()],
                        force: true,
                        no_force_cloud_conflict: no_force_cloud_conflict || !force,
                        preview,
                        gh194: false,
                        path,
                        wine_prefix: Default::default(),
                        api: Default::default(),
                        gui,
                        sort: Default::default(),
                        format,
                        compression,
                        compression_level,
                        full_limit,
                        differential_limit,
                        cloud_sync,
                        no_cloud_sync,
                        dump_registry: Default::default(),
                        include_disabled: Default::default(),
                        ask_downgrade,
                    },
                    no_manifest_update,
                    try_manifest_update,
                ) {
                    log::error!("Wrap failed to back up with: {:#?}", err);
                    ui::alert_with_error(
                        &games,
                        gui,
                        force_backup,
                        &TRANSLATOR.back_up_one_game_failed(game_name),
                        &err,
                    )?;
                    return Err(err);
                }
            }
        }
        Subcommand::Api { input } => {
            TRANSLATOR.set_language(Language::English);

            let manifest = match load_manifest(&config, &mut cache, no_manifest_update, try_manifest_update) {
                Ok(x) => x,
                Err(e) => {
                    api::abort_error(e);
                }
            };

            match api::process(input, &config, &manifest) {
                Ok(data) => {
                    let output = serde_json::to_string_pretty(&data).unwrap();
                    println!("{output}");
                }
                Err(e) => {
                    api::abort_message(e);
                }
            }
        }
        Subcommand::Schema { format, kind } => {
            let format = format.unwrap_or_default();
            let schema = match kind {
                parse::SchemaSubcommand::ApiInput => schemars::schema_for!(api::Input),
                parse::SchemaSubcommand::ApiOutput => schemars::schema_for!(api::Output),
                parse::SchemaSubcommand::GeneralOutput => schemars::schema_for!(report::ApiOutput),
                parse::SchemaSubcommand::Config => schemars::schema_for!(Config),
            };

            let serialized = match format {
                parse::SerializationFormat::Json => serde_json::to_string_pretty(&schema).unwrap(),
                parse::SerializationFormat::Yaml => serde_yaml::to_string(&schema).unwrap(),
            };
            println!("{serialized}");
        }
        Subcommand::Gui { .. } => {
            unreachable!("`gui` command must be handled in main");
        }
    }
    if failed {
        Err(Error::SomeEntriesFailed)
    } else {
        Ok(())
    }
}

fn configure_cloud(config: &mut Config, remote: Remote) -> Result<(), Error> {
    if let Some(old_remote) = config.cloud.remote.as_ref() {
        _ = Rclone::new(config.apps.rclone.clone(), old_remote.clone()).unconfigure_remote();
    }

    Rclone::new(config.apps.rclone.clone(), remote.clone())
        .configure_remote()
        .map_err(Error::UnableToConfigureCloud)?;

    config.cloud.remote = Some(remote);
    config.save();
    Ok(())
}

fn scan_progress_bar(length: u64) -> ProgressBar {
    let template = format!(
        "{} ({{elapsed_precise}}) {{wide_bar}} {}: {{pos}} / {{len}}",
        TRANSLATOR.scan_label(),
        TRANSLATOR.total_games()
    );
    let style = indicatif::ProgressStyle::default_bar()
        .template(&template)
        .expect("progress bar");
    let bar = ProgressBar::new(length).with_style(style);
    bar.enable_steady_tick(PROGRESS_BAR_REFRESH_INTERVAL);
    bar
}

fn cloud_progress_bar() -> ProgressBar {
    let template = format!(
        "{} ({{elapsed_precise}}) {{wide_bar}} {{msg}}",
        TRANSLATOR.cloud_label()
    );
    let style = indicatif::ProgressStyle::default_bar()
        .template(&template)
        .expect("progress bar");
    let bar = ProgressBar::new(100).with_style(style);
    bar.enable_steady_tick(PROGRESS_BAR_REFRESH_INTERVAL);
    bar
}

fn sync_cloud(
    config: &Config,
    local: &StrictPath,
    cloud: &str,
    sync: SyncDirection,
    finality: Finality,
    games: &[String],
) -> Result<Vec<CloudChange>, Error> {
    match finality {
        Finality::Preview => log::info!("checking cloud sync"),
        Finality::Final => log::info!("performing cloud sync"),
    }

    let remote = crate::cloud::validate_cloud_config(config, cloud)?;

    let games = if !games.is_empty() {
        let layout = BackupLayout::new(local.clone());
        let games: Vec<_> = games.iter().filter_map(|x| layout.game_folder(x).leaf()).collect();
        games
    } else {
        vec![]
    };

    let rclone = Rclone::new(config.apps.rclone.clone(), remote);
    let mut process = match rclone.sync(local, cloud, sync, finality, &games) {
        Ok(p) => p,
        Err(e) => return Err(Error::UnableToSynchronizeCloud(e)),
    };

    let interrupted = register_sigint();

    let progress_bar = cloud_progress_bar();
    let mut changes = vec![];
    loop {
        if interrupted.load(std::sync::atomic::Ordering::Relaxed) {
            if let Err(e) = process.kill() {
                eprintln!("Unable to stop Rclone: {e:?}");
            }
            std::process::exit(1);
        }

        let events = process.events();
        for event in events {
            match event {
                crate::cloud::RcloneProcessEvent::Progress { current, max } => {
                    progress_bar.set_length(max as u64);
                    progress_bar.set_position(current as u64);
                    progress_bar.set_message(TRANSLATOR.cloud_progress(current as u64, max as u64))
                }
                crate::cloud::RcloneProcessEvent::Change(change) => {
                    changes.push(change);
                }
            }
        }
        match process.succeeded() {
            Some(Ok(_)) => {
                unregister_sigint();
                return Ok(changes);
            }
            Some(Err(e)) => {
                unregister_sigint();
                progress_bar.finish_and_clear();
                return Err(Error::UnableToSynchronizeCloud(e));
            }
            None => (),
        }
    }
}
