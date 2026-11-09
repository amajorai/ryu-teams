//! Agent **teams**: a persisted, ordered collection of agents that can be
//! addressed as one unit (`@team` in chat) so a single message fans out to every
//! member.
//!
//! A team is *not* an agent — it is a collection of agent ids plus a
//! **coordination strategy** that decides how the members respond when the team
//! is called:
//!
//! - [`Coordination::Broadcast`] — every member answers the same prompt
//!   independently; no member sees another's reply.
//! - [`Coordination::RoundRobin`] — members answer in order, each seeing the
//!   prior members' replies folded into its prompt.
//! - [`Coordination::DebateSynthesis`] — members answer independently (round 1),
//!   then a designated **lead** agent reads every reply and produces one merged
//!   answer.
//! - [`Coordination::Router`] — a router picks the single best-suited member and
//!   routes only to it.
//!
//! The strategy is a per-team, swappable enum (Ryu's "nothing hardcoded"
//! principle): a team can be re-configured at any time without code changes, and
//! the orchestration that interprets it lives entirely in Core
//! (`sidecar::adapters::route_team_chat_stream`). This module owns only the
//! **persistence**; the typed records it reads and writes are the shared wire
//! contract, re-exported below from `ryu-teams-contracts`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use ryu_crypto::content_sqlite::{ColumnPlan, KeyKind};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::seal_keys::PluginKeys;
use crate::seal_sql::{collect_readable, open_store, opened, opened_opt, SqlSeal};

// ── Wire types ──────────────────────────────────────────────────────────────────

/// The three shapes that cross the loopback boundary — [`Coordination`],
/// [`TeamRecord`], [`CreateTeam`] — live in `ryu-teams-contracts`, NOT here.
///
/// Core's `@team` orchestration and `agent_builder.create_agent_team` speak the
/// same payloads this store persists, and neither side may link the other. A
/// shared, `serde`-only crate is the one place the kebab-case string table can be
/// declared once: `Coordination::as_str` is the SQL column value written at
/// [`TeamStore::create`] / [`TeamStore::update`] *and* the JSON the sidecar serves,
/// so a copy on each side of the wire would be two sources of truth for one
/// contract. Re-exported here so `ryu_teams::{Coordination, TeamRecord, CreateTeam}`
/// stays a valid path for existing consumers.
///
/// [`UpdateTeam`] below is deliberately NOT shared: it is this crate's own PATCH
/// shape and Core never names it.
pub use ryu_teams_contracts::{Coordination, CreateTeam, TeamRecord};

/// How a team's members respond when the team is called — the documentation-only
/// mirror of [`Coordination`], which cannot carry a `ToSchema` derive because
/// `ryu-teams-contracts` is `serde`-only by design (Core links it and must not
/// inherit `utoipa`).
///
/// This is what a caller writes into the `coordination` field of a create/update
/// body. The values are the same kebab-case strings the store persists.
// Pinned to the real enum by `coordination_schema_mirrors_the_real_wire_type`: every
// variant here must round-trip through `Coordination::from_str_lenient`/`as_str`, so a
// rename or removal on the contract side fails the tests instead of quietly handing
// the model a value the write path would reject.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CoordinationSchema {
    /// Every member answers the same prompt independently (the default).
    #[default]
    Broadcast,
    /// Members answer in order; each sees the prior members' replies.
    RoundRobin,
    /// Members answer independently, then the lead agent synthesizes one answer.
    DebateSynthesis,
    /// A router picks the single best-suited member and routes only to it.
    Router,
}

/// Fields a client may patch on update. Absent fields are left unchanged.
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
pub struct UpdateTeam {
    /// Rename the team. Omit to leave the name unchanged.
    #[serde(default)]
    pub name: Option<String>,
    /// Replace the description. Omit to leave it unchanged.
    #[serde(default)]
    pub description: Option<String>,
    /// Replace the full ordered member list. Use the member endpoints for
    /// incremental add/remove (e.g. drag-and-drop).
    #[serde(default)]
    pub members: Option<Vec<String>>,
    /// Switch the coordination strategy. Omit to leave it unchanged.
    // Inlined via the mirror so the model reads the four literal values; a `$ref`
    // buried inside the nullable wrapper is deeper than Core's importer resolves.
    #[serde(default)]
    #[schema(value_type = Option<CoordinationSchema>, inline)]
    pub coordination: Option<Coordination>,
    /// Patch the lead agent: an id sets it, an explicit `null` clears it, and
    /// omitting the field leaves it unchanged.
    // `Option<Option<String>>` is the serde tri-state; the schema states the JSON
    // shape a caller actually writes, which is one nullable string.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub lead_agent_id: Option<Option<String>>,
}

/// Deserialize a field so that a present `null` becomes `Some(None)` (clear) and
/// an absent field becomes `None` (leave unchanged) — the standard JSON-patch
/// tri-state for a nullable column.
fn double_option<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

// ── Store ────────────────────────────────────────────────────────────────────

/// SQLite-backed store for team config records. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct TeamStore {
    conn: Arc<Mutex<Connection>>,
    seal: SqlSeal,
}

/// The plugin id (the sealing key's scope).
pub const PLUGIN_ID: &str = "@ryu/teams";
/// The registered content-store name (part of every key derivation; never rename).
pub const STORE: &str = "teams.db";

/// The team columns, with the sealed name and description opened.
const TEAM_COLUMNS: &str = "id, ryu_open('teams.name', id, name) AS name, \
     ryu_open('teams.description', id, description) AS description, members, coordination, \
     lead_agent_id, created_at, updated_at";

/// The sealed columns (`encb:v1:`, bound to `teams.<column>` and the id). The
/// member list holds agent ids and stays plaintext, like the strategy.
pub fn column_plans() -> Result<Vec<ColumnPlan>> {
    ["name", "description"]
        .iter()
        .map(|column| ColumnPlan::text("teams", column, "id", KeyKind::Text))
        .collect()
}

impl TeamStore {
    /// Open (creating if needed) the teams DB at `path` (the sidecar passes
    /// `<data dir>/plugin-data/@ryu+teams/teams.db`) and run the schema migration. The data-dir path is
    /// injected by the host so this crate has ZERO dependency on `apps/core`.
    ///
    /// The sealing key is resolved with the DB's directory as the plugin data
    /// dir (see [`crate::seal_keys`]).
    pub fn open(path: PathBuf) -> Result<Self> {
        let dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let keys = PluginKeys::resolve(PLUGIN_ID, &dir)?;
        Self::open_with_keys(path, &keys)
    }

    /// [`Self::open`] with explicit keys.
    pub fn open_with_keys(path: PathBuf, keys: &PluginKeys) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("creating parent dir for teams.db")?;
        }
        let mut conn = Connection::open(&path)
            .with_context(|| format!("opening teams db at {}", path.display()))?;
        ryu_crypto::content_sqlite::enable_secure_delete(&conn)?;
        Self::migrate(&conn)?;
        let seal = open_store(&mut conn, keys, STORE, &column_plans()?, Some(&path))
            .context("sealing teams.db")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            seal,
        })
    }

    /// In-memory store sealing under a random key, used by this crate's tests,
    /// so it is a plain `pub fn`, not `#[cfg(test)]`.
    pub fn open_in_memory() -> Result<Self> {
        use base64::Engine as _;
        use rand::RngCore as _;
        let mut root = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut root);
        let keys = PluginKeys::sealing(
            ryu_crypto::PluginCipher::from_subkey_b64(
                PLUGIN_ID,
                &base64::engine::general_purpose::STANDARD.encode(root),
            )?,
            crate::seal_keys::KeySource::Delivered,
        );
        let mut conn = Connection::open_in_memory()?;
        ryu_crypto::content_sqlite::enable_secure_delete(&conn)?;
        Self::migrate(&conn)?;
        let seal = open_store(&mut conn, &keys, STORE, &column_plans()?, None)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            seal,
        })
    }

    /// The sealing health item: `{state, detail?, unreadable_rows}`.
    pub fn seal_health(&self) -> serde_json::Value {
        self.seal.health()
    }

    fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS teams (
                id            TEXT PRIMARY KEY,
                name          TEXT NOT NULL,
                description   TEXT,
                members       TEXT NOT NULL DEFAULT '[]',
                coordination  TEXT NOT NULL DEFAULT 'broadcast',
                lead_agent_id TEXT,
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL
            );",
        )
        .context("running teams schema migration")?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<TeamRecord>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(&format!(
            "SELECT {TEAM_COLUMNS}
             FROM teams ORDER BY created_at ASC"
        ))?;
        let rows = stmt.query_map([], row_to_record)?;
        let rows = collect_readable(rows)?;
        Ok(rows)
    }

    pub async fn get(&self, id: &str) -> Result<Option<TeamRecord>> {
        let conn = self.conn.lock().await;
        let record = conn
            .query_row(
                &format!("SELECT {TEAM_COLUMNS} FROM teams WHERE id = ?1"),
                params![id],
                row_to_record,
            )
            .optional()?;
        Ok(record)
    }

    pub async fn create(&self, input: CreateTeam) -> Result<TeamRecord> {
        let id = format!("team_{}", uuid::Uuid::new_v4().simple());
        let now = chrono::Utc::now().to_rfc3339();
        let members_json =
            serde_json::to_string(&input.members).unwrap_or_else(|_| "[]".to_owned());
        {
            let conn = self.conn.lock().await;
            conn.execute(
                "INSERT INTO teams
                    (id, name, description, members, coordination, lead_agent_id,
                     created_at, updated_at)
                 VALUES (?1, ryu_seal('teams.name', ?1, ?2),
                         ryu_seal('teams.description', ?1, ?3), ?4, ?5, ?6, ?7, ?7)",
                params![
                    id,
                    input.name,
                    input.description,
                    members_json,
                    input.coordination.as_str(),
                    input.lead_agent_id,
                    now,
                ],
            )?;
        }
        self.get(&id)
            .await?
            .context("team vanished immediately after insert")
    }

    pub async fn update(&self, id: &str, patch: UpdateTeam) -> Result<Option<TeamRecord>> {
        // Keep the complete read-modify-write under one lock. Releasing the lock
        // between the read and write lets two independent PATCH/member requests
        // silently overwrite each other's fields or roster entries.
        let conn = self.conn.lock().await;
        let Some(mut record) = get_record(&conn, id)? else {
            return Ok(None);
        };
        if let Some(name) = patch.name {
            record.name = name;
        }
        if let Some(description) = patch.description {
            record.description = Some(description);
        }
        if let Some(members) = patch.members {
            record.members = members;
        }
        if let Some(coordination) = patch.coordination {
            record.coordination = coordination;
        }
        if let Some(lead) = patch.lead_agent_id {
            record.lead_agent_id = lead;
        }
        let now = chrono::Utc::now().to_rfc3339();
        let members_json =
            serde_json::to_string(&record.members).unwrap_or_else(|_| "[]".to_owned());
        conn.execute(
            "UPDATE teams
                 SET name = ryu_seal('teams.name', ?7, ?1),
                     description = ryu_seal('teams.description', ?7, ?2),
                     members = ?3, coordination = ?4,
                     lead_agent_id = ?5, updated_at = ?6
                 WHERE id = ?7",
            params![
                record.name,
                record.description,
                members_json,
                record.coordination.as_str(),
                record.lead_agent_id,
                now,
                id,
            ],
        )?;
        get_record(&conn, id)
    }

    pub async fn delete(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let deleted = conn.execute("DELETE FROM teams WHERE id = ?1", params![id])?;
        Ok(deleted > 0)
    }

    /// Append an agent to a team's member list (idempotent). Returns the updated
    /// record, or `None` if the team does not exist. Used by drag-and-drop.
    pub async fn add_member(&self, team_id: &str, agent_id: &str) -> Result<Option<TeamRecord>> {
        let conn = self.conn.lock().await;
        let Some(mut record) = get_record(&conn, team_id)? else {
            return Ok(None);
        };
        if record.members.iter().any(|m| m == agent_id) {
            return Ok(Some(record));
        }
        record.members.push(agent_id.to_owned());
        write_members(&conn, team_id, &record.members)?;
        get_record(&conn, team_id)
    }

    /// Remove an agent from a team's member list (idempotent). Returns the
    /// updated record, or `None` if the team does not exist.
    pub async fn remove_member(&self, team_id: &str, agent_id: &str) -> Result<Option<TeamRecord>> {
        let conn = self.conn.lock().await;
        let Some(mut record) = get_record(&conn, team_id)? else {
            return Ok(None);
        };
        record.members.retain(|member| member != agent_id);
        write_members(&conn, team_id, &record.members)?;
        get_record(&conn, team_id)
    }
}

fn get_record(conn: &Connection, id: &str) -> Result<Option<TeamRecord>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {TEAM_COLUMNS}
             FROM teams WHERE id = ?1"
            ),
            params![id],
            row_to_record,
        )
        .optional()?)
}

fn write_members(conn: &Connection, id: &str, members: &[String]) -> Result<()> {
    let members_json = serde_json::to_string(members).context("serializing team members")?;
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE teams SET members = ?1, updated_at = ?2 WHERE id = ?3",
        params![members_json, now, id],
    )?;
    Ok(())
}

/// Map a `teams` row to a [`TeamRecord`]. Column order must match the SELECTs.
fn row_to_record(row: &rusqlite::Row) -> rusqlite::Result<TeamRecord> {
    let members_raw: String = row.get(3)?;
    let coordination_raw: String = row.get(4)?;
    Ok(TeamRecord {
        id: row.get(0)?,
        name: opened(row, 1)?,
        description: opened_opt(row, 2)?,
        members: serde_json::from_str(&members_raw).unwrap_or_default(),
        coordination: Coordination::from_str_lenient(&coordination_raw),
        lead_agent_id: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TeamStore {
        TeamStore::open_in_memory().unwrap()
    }

    #[tokio::test]
    async fn create_list_get_roundtrip() {
        let s = store();
        let t = s
            .create(CreateTeam {
                name: "Research".to_owned(),
                description: Some("paper readers".to_owned()),
                members: vec!["acp:claude".to_owned(), "ryu".to_owned()],
                coordination: Coordination::Broadcast,
                lead_agent_id: None,
            })
            .await
            .unwrap();
        assert!(t.id.starts_with("team_"));
        assert_eq!(t.members, vec!["acp:claude", "ryu"]);
        assert_eq!(t.coordination, Coordination::Broadcast);

        let listed = s.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        let got = s.get(&t.id).await.unwrap().unwrap();
        assert_eq!(got.name, "Research");
        assert_eq!(got.description.as_deref(), Some("paper readers"));
    }

    #[tokio::test]
    async fn add_remove_member_idempotent() {
        let s = store();
        let t = s
            .create(CreateTeam {
                name: "T".to_owned(),
                ..Default::default()
            })
            .await
            .unwrap();
        // Add twice — second is a no-op.
        s.add_member(&t.id, "ryu").await.unwrap();
        let r = s.add_member(&t.id, "ryu").await.unwrap().unwrap();
        assert_eq!(r.members, vec!["ryu"]);
        // Remove (idempotent on a missing member).
        let r = s.remove_member(&t.id, "ryu").await.unwrap().unwrap();
        assert!(r.members.is_empty());
        let r = s.remove_member(&t.id, "ryu").await.unwrap().unwrap();
        assert!(r.members.is_empty());
    }

    #[tokio::test]
    async fn update_patches_only_present_fields() {
        let s = store();
        let t = s
            .create(CreateTeam {
                name: "Old".to_owned(),
                members: vec!["a".to_owned()],
                coordination: Coordination::Broadcast,
                ..Default::default()
            })
            .await
            .unwrap();
        let updated = s
            .update(
                &t.id,
                UpdateTeam {
                    coordination: Some(Coordination::DebateSynthesis),
                    lead_agent_id: Some(Some("a".to_owned())),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        // name + members untouched; coordination + lead changed.
        assert_eq!(updated.name, "Old");
        assert_eq!(updated.members, vec!["a"]);
        assert_eq!(updated.coordination, Coordination::DebateSynthesis);
        assert_eq!(updated.lead_agent_id.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn coordination_string_roundtrip() {
        for c in [
            Coordination::Broadcast,
            Coordination::RoundRobin,
            Coordination::DebateSynthesis,
            Coordination::Router,
        ] {
            assert_eq!(Coordination::from_str_lenient(c.as_str()), c);
        }
        // Unknown values fall back to Broadcast, never panic.
        assert_eq!(
            Coordination::from_str_lenient("nonsense"),
            Coordination::Broadcast
        );
    }

    #[tokio::test]
    async fn delete_removes_team() {
        let s = store();
        let t = s
            .create(CreateTeam {
                name: "Gone".to_owned(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(s.delete(&t.id).await.unwrap());
        assert!(s.get(&t.id).await.unwrap().is_none());
        assert!(!s.delete(&t.id).await.unwrap());
    }
}
