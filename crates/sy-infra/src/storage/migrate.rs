//! The in house migration runner (spec 0002, *Value sourcing*: "Migrate").
//!
//! Migration `n` is `MIGRATIONS[n - 1]`. Each pending one runs in its own
//! transaction that also sets `PRAGMA user_version = n`, so a failure rolls
//! back cleanly and leaves the version unchanged.
//!
//! **The rebuild rule.** SQLite can't alter a `CHECK`, so widening one copies
//! the table. A migration file whose first line is `-- rebuild` makes the
//! runner turn `foreign_keys` off before its transaction (the pragma is
//! ignored inside one, and a `DROP TABLE` with it on would cascade delete
//! every child row), run `PRAGMA foreign_key_check` before committing (any
//! finding rolls back), then turn `foreign_keys` on again.

use rusqlite::Connection;

use super::OpenError;

/// One schema migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    /// The file name, `NNNN_name.sql`.
    pub name: &'static str,
    /// The SQL.
    pub sql: &'static str,
    /// Run under the rebuild rule (the file starts with `-- rebuild`).
    pub rebuild: bool,
}

/// Every migration, in order. Append only.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        name: "0001_spine.sql",
        sql: include_str!("../../migrations/0001_spine.sql"),
        rebuild: false,
    },
    Migration {
        name: "0002_approval.sql",
        sql: include_str!("../../migrations/0002_approval.sql"),
        rebuild: false,
    },
];

/// The newest schema version a list of migrations reaches.
pub fn latest(migrations: &[Migration]) -> i64 {
    i64::try_from(migrations.len()).unwrap_or(i64::MAX)
}

/// `PRAGMA user_version`.
pub fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Apply every migration after the database's `user_version`, in order.
/// Returns the versions applied. Refuses a database newer than `migrations`.
pub fn run(conn: &mut Connection, migrations: &[Migration]) -> Result<Vec<i64>, OpenError> {
    let unavailable = |e: rusqlite::Error| OpenError::Unavailable(e.to_string());
    let current = user_version(conn).map_err(unavailable)?;
    let known = latest(migrations);
    if current > known {
        return Err(OpenError::NewerSchema {
            found: current,
            known,
        });
    }
    let mut applied = Vec::new();
    for (index, migration) in migrations.iter().enumerate() {
        let version = i64::try_from(index + 1).unwrap_or(i64::MAX);
        if version <= current {
            continue;
        }
        apply(conn, version, migration)
            .map_err(|message| OpenError::Migration { version, message })?;
        tracing::info!(version, name = migration.name, "migration applied");
        applied.push(version);
    }
    Ok(applied)
}

fn apply(conn: &mut Connection, version: i64, migration: &Migration) -> Result<(), String> {
    let text = |e: rusqlite::Error| e.to_string();
    if migration.rebuild {
        conn.pragma_update(None, "foreign_keys", false)
            .map_err(text)?;
    }
    let result = (|| {
        let tx = conn.transaction().map_err(text)?;
        tx.execute_batch(migration.sql).map_err(text)?;
        if migration.rebuild {
            let findings: i64 = tx
                .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })
                .map_err(text)?;
            if findings > 0 {
                return Err(format!("{findings} foreign key problems after the rebuild"));
            }
        }
        tx.pragma_update(None, "user_version", version)
            .map_err(text)?;
        tx.commit().map_err(text)
    })();
    if migration.rebuild {
        let restored = conn.pragma_update(None, "foreign_keys", true).map_err(text);
        result.and(restored)
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_files_should_be_numbered_from_1_with_no_gaps_and_all_listed() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
        let mut files: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        files.sort();
        let numbered = files
            .iter()
            .enumerate()
            .all(|(i, name)| name.starts_with(&format!("{:04}_", i + 1)));
        let listed: Vec<&str> = MIGRATIONS.iter().map(|m| m.name).collect();
        assert!(numbered, "gap or bad name in {files:?}");
        assert_eq!(files, listed);
    }

    #[test]
    fn rebuild_flag_should_match_the_file_marker() {
        let mismatched: Vec<_> = MIGRATIONS
            .iter()
            .filter(|m| m.sql.starts_with("-- rebuild") != m.rebuild)
            .map(|m| m.name)
            .collect();
        assert!(mismatched.is_empty(), "{mismatched:?}");
    }

    #[test]
    fn fresh_database_should_reach_the_newest_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        let applied = run(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(
            (applied.len() as i64, user_version(&conn).unwrap()),
            (latest(MIGRATIONS), latest(MIGRATIONS))
        );
    }

    #[test]
    fn running_twice_should_apply_nothing_the_second_time() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn, MIGRATIONS).unwrap();
        assert!(run(&mut conn, MIGRATIONS).unwrap().is_empty());
    }

    #[test]
    fn newer_database_should_be_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
        assert_eq!(
            run(&mut conn, MIGRATIONS),
            Err(OpenError::NewerSchema {
                found: 999,
                known: latest(MIGRATIONS)
            })
        );
    }

    #[test]
    fn failing_migration_should_roll_back_and_keep_the_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn, MIGRATIONS).unwrap();
        let broken = [
            MIGRATIONS,
            &[Migration {
                name: "9999_broken.sql",
                sql: "CREATE TABLE half (x INTEGER) STRICT; INSERT INTO nowhere VALUES (1);",
                rebuild: false,
            }],
        ]
        .concat();
        let result = run(&mut conn, &broken);
        let half_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'half'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let next = latest(MIGRATIONS) + 1;
        assert!(matches!(
            result,
            Err(OpenError::Migration { version, .. }) if version == next
        ));
        assert_eq!(
            (user_version(&conn).unwrap(), half_exists),
            (latest(MIGRATIONS), 0)
        );
    }
}
