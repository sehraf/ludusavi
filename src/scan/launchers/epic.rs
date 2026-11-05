use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use crate::{
    prelude::StrictPath,
    resource::{config::root, manifest::Os},
    scan::{TitleFinder, launchers::LauncherGame},
};

const INSTALLATIONS_PATH: &str = "installations.jsonl";
const CATALOG_CACHE_PATH: &str = "cache/prod/catalog-cache.json";

pub fn has_installations(root: &root::Epic) -> bool {
    root.path.joined(INSTALLATIONS_PATH).is_file()
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Installation {
    installation_id: Option<String>,
    complete: Option<bool>,
    deleted: Option<bool>,
    dir: Option<StrictPath>,
    item_id: Option<String>,
    artifact_id: Option<String>,
    launch_executable: Option<String>,
    platform: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct CatalogCache {
    #[serde(default)]
    items: HashMap<String, CatalogEntry>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct CatalogEntry {
    item: Option<CatalogItem>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct CatalogItem {
    title: Option<String>,
}

/// Map from item ID to the game's title in the store.
fn load_catalog_titles(root: &root::Epic) -> HashMap<String, String> {
    let file = root.path.joined(CATALOG_CACHE_PATH);
    let content = match file.try_read() {
        Ok(content) => content,
        Err(e) => {
            log::debug!("Epic: Unable to read catalog cache: {file:?} | {e:?}");
            return HashMap::new();
        }
    };

    match serde_json::from_str::<CatalogCache>(&content) {
        Ok(cache) => cache
            .items
            .into_iter()
            .filter_map(|(id, entry)| Some((id, entry.item?.title?)))
            .collect(),
        Err(e) => {
            log::warn!("Epic: Unable to parse catalog cache: {file:?} | {e:?}");
            HashMap::new()
        }
    }
}

pub fn scan(root: &root::Epic, title_finder: &TitleFinder) -> HashMap<String, HashSet<LauncherGame>> {
    let file = root.path.joined(INSTALLATIONS_PATH);
    let content = match file.try_read() {
        Ok(content) => content,
        Err(e) => {
            log::debug!("Epic: Unable to read installations list: {file:?} | {e:?}");
            return HashMap::new();
        }
    };

    // The file is an append-only journal: each line records a state change for an installation,
    // and the launcher keeps the lines in the order they were written (even when it rewrites the file).
    // We replay it from top to bottom, so the last relevant line for each installation wins.
    // We can't sort by `writtenAt` instead, because a `deleted` line may share
    // its timestamp with the line immediately before it.
    //
    // An incomplete line (e.g., during an update) doesn't replace the last complete one,
    // since the game is still installed until a `deleted` line appears.
    let mut installations = HashMap::<String, Installation>::new();

    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        let installation = match serde_json::from_str::<Installation>(line) {
            Ok(installation) => installation,
            Err(e) => {
                log::warn!("Epic: Unable to parse installations list line {}: {e:?}", index + 1);
                continue;
            }
        };

        let Some(installation_id) = installation.installation_id.as_ref() else {
            continue;
        };

        if installation.deleted == Some(true) {
            installations.remove(installation_id);
        } else if installation.complete == Some(true) {
            installations.insert(installation_id.clone(), installation);
        }
    }

    let catalog_titles = load_catalog_titles(root);
    let mut out = HashMap::<String, HashSet<LauncherGame>>::new();

    for installation in installations.into_values() {
        let mut candidates = Vec::new();
        let catalog_title = installation.item_id.as_ref().and_then(|id| catalog_titles.get(id));
        if let Some(title) = catalog_title {
            candidates.push(title.clone());
        }
        if let Some(executable) = installation.launch_executable.as_deref() {
            let filename = executable.rsplit(['/', '\\']).next();
            if let Some(title) = filename.and_then(|filename| Path::new(filename).file_stem()?.to_str()) {
                candidates.push(title.to_string());
            }
        }
        if let Some(title) = installation.dir.as_ref().and_then(StrictPath::leaf) {
            candidates.push(title);
        }
        let Some(title) = catalog_title
            .and_then(|title| title_finder.find_one_by_name(title))
            .or_else(|| {
                candidates
                    .iter()
                    .find_map(|candidate| title_finder.find_one_by_normalized_name(candidate))
            })
        else {
            log::trace!("Epic: Ignoring installation with unrecognized title: {:?}", candidates);
            continue;
        };

        let prefix = installation
            .artifact_id
            .as_ref()
            .map(|id| root.path.joined(format!("compat/compatdata/{id}/pfx")));
        let game = LauncherGame {
            install_dir: installation.dir,
            prefix,
            platform: installation.platform.as_deref().map(Os::from),
        };
        out.entry(title).or_default().insert(game);
    }

    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use velcro::{hash_map, hash_set};

    use super::*;
    use crate::{
        resource::{ResourceFile, manifest::Manifest},
        testing::repo,
    };

    fn title_finder() -> TitleFinder {
        let manifest = Manifest::load_from_string(
            r#"
            Nova Lands:
              files:
                <base>/file1.txt: {}
            Flotsam:
              files:
                <base>/file1.txt: {}
            Slime Rancher:
              files:
                <base>/file1.txt: {}
            Dead Island 2:
              files:
                <base>/file1.txt: {}
            20XX:
              files:
                <base>/file1.txt: {}
            "#,
        )
        .unwrap();
        TitleFinder::new(&Default::default(), &manifest, Default::default())
    }

    #[test]
    fn scan_finds_nothing_when_folder_does_not_exist() {
        let root = root::Epic {
            path: format!("{}/tests/nonexistent", repo()).into(),
        };
        assert_eq!(HashMap::new(), scan(&root, &title_finder()));
    }

    #[test]
    fn scan_keeps_games_during_updates_and_ignores_unfinished_reinstalls() {
        let root = root::Epic {
            path: format!("{}/tests/launchers/epic-journal", repo()).into(),
        };
        assert_eq!(
            hash_map! {
                "Flotsam".to_string(): hash_set![LauncherGame {
                    install_dir: Some(StrictPath::new("/home/user/Games/Flotsam")),
                    prefix: Some(StrictPath::new(
                        format!("{}/tests/launchers/epic-journal/compat/compatdata/b1cb2fc221124bf88472bc1be0de2034/pfx", repo())
                    )),
                    platform: Some(Os::Windows),
                }],
            },
            scan(&root, &title_finder()),
        );
    }

    #[test]
    fn scan_groups_installations_and_ignores_deleted_games() {
        let root = root::Epic {
            path: format!("{}/tests/launchers/epic", repo()).into(),
        };
        assert_eq!(
            hash_map! {
                "Nova Lands".to_string(): hash_set![LauncherGame {
                    install_dir: Some(StrictPath::new("/home/user/Games/NovaLandsNzKp8")),
                    prefix: Some(StrictPath::new(
                        format!("{}/tests/launchers/epic/compat/compatdata/487bfeacfebe4d2a921b0b1478ad6625/pfx", repo())
                    )),
                    platform: Some(Os::Windows),
                }],
                "Flotsam".to_string(): hash_set![LauncherGame {
                    install_dir: Some(StrictPath::new("/home/user/Games/Flotsam")),
                    prefix: Some(StrictPath::new(
                        format!("{}/tests/launchers/epic/compat/compatdata/b1cb2fc221124bf88472bc1be0de2034/pfx", repo())
                    )),
                    platform: Some(Os::Windows),
                }],
                "20XX".to_string(): hash_set![LauncherGame {
                    install_dir: Some(StrictPath::new("/home/user/Games/TwentyXX")),
                    prefix: Some(StrictPath::new(
                        format!("{}/tests/launchers/epic/compat/compatdata/Quail/pfx", repo())
                    )),
                    platform: Some(Os::Windows),
                }],
                "Slime Rancher".to_string(): hash_set![LauncherGame {
                    install_dir: Some(StrictPath::new("/home/user/Games/SlimeRancher")),
                    prefix: Some(StrictPath::new(
                        format!("{}/tests/launchers/epic/compat/compatdata/Corydalis/pfx", repo())
                    )),
                    platform: Some(Os::Windows),
                }],
            },
            scan(&root, &title_finder()),
        );
    }
}
