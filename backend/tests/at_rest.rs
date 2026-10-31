//! Team names and descriptions at rest: sealed per value in `teams.db`,
//! legacy plaintext sealed once on open, unreadable rows skipped and counted,
//! other keys refused.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use ryu_crypto::{ContentCipher, PluginCipher};
use ryu_teams::{
    seal_keys::{KeySource, PluginKeys},
    store::{column_plans, TeamStore, PLUGIN_ID, STORE},
};

const NAME: &str = "Zebra launch crew";
const DESCRIPTION: &str = "quokka-secret-instructions";

fn keys(n: u8) -> PluginKeys {
    PluginKeys::sealing(
        PluginCipher::from_subkey_b64(
            PLUGIN_ID,
            &base64::engine::general_purpose::STANDARD.encode([n; 32]),
        )
        .unwrap(),
        KeySource::Delivered,
    )
}

fn db() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "ryu-teams-at-rest-{}",
            uuid::Uuid::new_v4().simple()
        ))
        .join(STORE)
}

fn on_disk(path: &Path, needle: &str) -> bool {
    let mut bytes = std::fs::read(path).unwrap_or_default();
    bytes.extend(std::fs::read(path.with_extension("db-wal")).unwrap_or_default());
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

async fn seed(store: &TeamStore) -> String {
    store
        .create(
            serde_json::from_value(serde_json::json!({
                "name": NAME,
                "description": DESCRIPTION,
                "members": ["agent_a", "agent_b"],
            }))
            .unwrap(),
        )
        .await
        .unwrap()
        .id
}

fn raw_names(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn.prepare("SELECT name FROM teams ORDER BY id").unwrap();
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows
}

#[tokio::test]
async fn names_and_descriptions_are_sealed_on_disk_and_read_back() {
    let path = db();
    let store = TeamStore::open_with_keys(path.clone(), &keys(1)).unwrap();
    let id = seed(&store).await;
    let got = store.get(&id).await.unwrap().unwrap();
    assert_eq!(got.name, NAME);
    assert_eq!(got.description.as_deref(), Some(DESCRIPTION));
    assert_eq!(got.members, ["agent_a", "agent_b"]);
    drop(store);
    assert!(!on_disk(&path, NAME));
    assert!(!on_disk(&path, DESCRIPTION));
    let conn = rusqlite::Connection::open(&path).unwrap();
    let cipher = keys(1).content_cipher(STORE).unwrap();
    ryu_crypto::content_sqlite::assert_all_open(&conn, &cipher, &column_plans().unwrap()).unwrap();
}

#[tokio::test]
async fn legacy_teams_are_sealed_once_and_still_read() {
    let path = db();
    let id = seed(&TeamStore::open_with_keys(path.clone(), &PluginKeys::compat()).unwrap()).await;
    assert!(on_disk(&path, DESCRIPTION));
    let store = TeamStore::open_with_keys(path.clone(), &keys(2)).unwrap();
    assert_eq!(store.get(&id).await.unwrap().unwrap().name, NAME);
    drop(store);
    assert!(!on_disk(&path, NAME));
    assert!(!on_disk(&path, DESCRIPTION));
    let first = raw_names(&path);
    assert!(first.iter().all(|name| ContentCipher::is_sealed_text(name)));
    drop(TeamStore::open_with_keys(path.clone(), &keys(2)).unwrap());
    assert_eq!(raw_names(&path), first);
}

#[tokio::test]
async fn a_copied_name_is_skipped_and_other_keys_refuse_writes() {
    let path = db();
    let store = TeamStore::open_with_keys(path.clone(), &keys(3)).unwrap();
    let first = seed(&store).await;
    let second = seed(&store).await;
    drop(store);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE teams SET name = (SELECT name FROM teams WHERE id = ?1) WHERE id = ?2",
            [&first, &second],
        )
        .unwrap();
    let store = TeamStore::open_with_keys(path.clone(), &keys(3)).unwrap();
    let listed = store.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, first);
    assert!(store.get(&second).await.is_err());
    assert_eq!(store.seal_health()["unreadable_rows"], 2);
    drop(store);
    for other in [keys(4), PluginKeys::compat()] {
        let wrong = TeamStore::open_with_keys(path.clone(), &other).unwrap();
        assert_eq!(wrong.seal_health()["state"], "unhealthy");
        assert!(wrong
            .create(serde_json::from_value(serde_json::json!({ "name": "refused" })).unwrap())
            .await
            .is_err());
    }
}
