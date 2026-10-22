use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::{IndexReport, text, workspace};

const APPLICATION_ID: i32 = 0x534f4e52;
const SCHEMA_VERSION: i32 = 3;

const SCHEMA: &str = "
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, hash TEXT NOT NULL, parse_errors INTEGER NOT NULL);
CREATE TABLE locations (
    id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    symbol TEXT, kind TEXT NOT NULL, definition INTEGER,
    parent_id INTEGER REFERENCES locations(id) ON DELETE CASCADE,
    start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
    name_start INTEGER NOT NULL, name_end INTEGER NOT NULL, is_test INTEGER NOT NULL
);
CREATE INDEX locations_file ON locations(file_id);
CREATE INDEX locations_parent ON locations(parent_id);
CREATE TABLE regions (
    id INTEGER PRIMARY KEY, location_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    start_line INTEGER NOT NULL, end_line INTEGER NOT NULL, source TEXT NOT NULL,
    embedding_text TEXT NOT NULL, view TEXT NOT NULL DEFAULT 'combined'
);
CREATE INDEX regions_location ON regions(location_id);
CREATE INDEX regions_view ON regions(view);
CREATE TABLE embeddings (
    region_id INTEGER PRIMARY KEY REFERENCES regions(id) ON DELETE CASCADE,
    vector BLOB NOT NULL
);
CREATE VIRTUAL TABLE search USING fts5(name, path, docs, body, tokenize = \"unicode61 tokenchars '_'\");
CREATE TRIGGER delete_region AFTER DELETE ON regions BEGIN
    DELETE FROM search WHERE rowid = old.id;
END;
CREATE TABLE semantic_edges (
    from_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    to_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    kind TEXT NOT NULL, path TEXT NOT NULL, line INTEGER NOT NULL,
    start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
    PRIMARY KEY(from_id, to_id, kind, path, start_byte, end_byte)
);
CREATE INDEX semantic_edges_to ON semantic_edges(to_id);
";

pub(crate) fn open(db: &Path, writable: bool) -> Result<Connection> {
    if writable {
        if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
    } else {
        ensure!(
            db.is_file(),
            "index not found at {}; run `sonar index <repository>` first",
            db.display()
        );
    }
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut conn = Connection::open_with_flags(db, flags)
        .with_context(|| format!("opening {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let app_id: i32 = conn.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if writable && app_id == 0 && version == 0 {
        let tables: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            tables == 0,
            "refusing to use an existing non-Sonar database"
        );
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
    } else if writable && app_id == APPLICATION_ID && (1..SCHEMA_VERSION).contains(&version) {
        let tx = conn.transaction()?;
        if version == 1 {
            tx.execute_batch("ALTER TABLE regions ADD COLUMN embedding_text TEXT NOT NULL DEFAULT '';
            CREATE TABLE embeddings (region_id INTEGER PRIMARY KEY REFERENCES regions(id) ON DELETE CASCADE, vector BLOB NOT NULL);")?;
        }
        tx.execute_batch(
            "ALTER TABLE locations ADD COLUMN start_byte INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE locations ADD COLUMN end_byte INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE locations ADD COLUMN name_start INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE locations ADD COLUMN name_end INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE locations ADD COLUMN is_test INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE regions ADD COLUMN view TEXT NOT NULL DEFAULT 'combined';
            CREATE INDEX regions_view ON regions(view);
            CREATE TABLE semantic_edges (
                from_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
                to_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
                kind TEXT NOT NULL, path TEXT NOT NULL, line INTEGER NOT NULL,
                start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
                PRIMARY KEY(from_id, to_id, kind, path, start_byte, end_byte));
            CREATE INDEX semantic_edges_to ON semantic_edges(to_id);
            DELETE FROM embeddings;
            DELETE FROM metadata WHERE key LIKE 'embedding_%';",
        )?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
    } else {
        ensure!(
            app_id == APPLICATION_ID && version == SCHEMA_VERSION,
            "unsupported index database; run `sonar index` to upgrade an older Sonar index, or use a new --db path"
        );
    }
    if writable {
        // Long embedding transactions must not prevent readers from using the previous generation.
        conn.pragma_update(None, "journal_mode", "WAL")?;
    }
    Ok(conn)
}

pub(crate) fn metadata(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

pub(crate) fn file_hashes(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let mut stmt = conn.prepare("SELECT path, hash FROM files ORDER BY path")?;
    Ok(stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

/// Update an index atomically. Failed scans or parses leave the previous generation intact.
pub fn index(root: &Path, db: &Path) -> Result<IndexReport> {
    index_with(root, db, crate::Chunking::Lines)
}

pub fn index_with(root: &Path, db: &Path, chunking: crate::Chunking) -> Result<IndexReport> {
    index_with_resources(root, db, chunking, crate::ResourcePolicy::None)
}

pub fn index_with_resources(
    root: &Path,
    db: &Path,
    chunking: crate::Chunking,
    resources: crate::ResourcePolicy,
) -> Result<IndexReport> {
    let started = Instant::now();
    let root = root
        .canonicalize()
        .with_context(|| format!("opening repository {}", root.display()))?;
    let root_string = root
        .to_str()
        .context("repository root must be valid UTF-8")?;
    let mut conn = open(db, true)?;
    // Serialize writers before comparing generations, so a concurrent index cannot overwrite newer state.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(previous_root) = metadata(&tx, "root")? {
        ensure!(
            previous_root == root_string,
            "this database belongs to {previous_root}; choose another --db path"
        );
    }
    let files = workspace::scan(&root, resources)?;
    let snapshot = workspace::snapshot(&files);
    let old = file_hashes(&tx)?;
    let current: BTreeSet<_> = files.iter().map(|file| file.path.as_str()).collect();
    let removed: Vec<_> = old
        .keys()
        .filter(|path| !current.contains(path.as_str()))
        .collect();
    let extraction_version = if chunking == crate::Chunking::Ast {
        "7-typescript-ast"
    } else {
        "7-typescript"
    };
    let reparse = metadata(&tx, "extraction_version")?.as_deref() != Some(extraction_version);
    let changed: Vec<_> = files
        .iter()
        .filter(|file| reparse || old.get(&file.path) != Some(&file.hash))
        .collect();
    if reparse || !changed.is_empty() || !removed.is_empty() {
        // Even an extractor-only rebuild replaces location IDs under the same source snapshot.
        tx.execute("DELETE FROM semantic_edges", [])?;
        tx.execute("DELETE FROM metadata WHERE key='usage_policy'", [])?;
        tx.execute("DELETE FROM metadata WHERE key='quantity_policy'", [])?;
        tx.execute("DELETE FROM metadata WHERE key='semantic_snapshot'", [])?;
        tx.execute(
            "DELETE FROM metadata WHERE key='typescript_semantic_snapshot'",
            [],
        )?;
        tx.execute("DELETE FROM metadata WHERE key='navigation_policy'", [])?;
    }
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(8);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()?;
    let parsed: Vec<_> = pool.install(|| {
        changed
            .par_iter()
            .map(|file| {
                crate::languages::extract(&file.path, &file.source, chunking)
                    .with_context(|| format!("parsing {}", file.path))
            })
            .collect::<Result<_>>()
    })?;

    for path in &removed {
        tx.execute("DELETE FROM files WHERE path = ?1", [path])?;
    }
    for (file, parsed) in changed.iter().zip(parsed) {
        tx.execute("DELETE FROM files WHERE path = ?1", [&file.path])?;
        tx.execute(
            "INSERT INTO files(path, hash, parse_errors) VALUES (?1, ?2, ?3)",
            params![file.path, file.hash, parsed.has_errors],
        )?;
        let file_id = tx.last_insert_rowid();
        let path_text = text::searchable(&file.path);
        let mut location_ids = Vec::new();
        let has_declarations = parsed.locations.len() > 1;
        for location in parsed.locations {
            let parent_id = location.parent.map(|parent| location_ids[parent]);
            tx.execute("INSERT INTO locations(file_id, symbol, kind, definition, parent_id, start_byte, end_byte, name_start, name_end, is_test) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![file_id, location.symbol, location.kind, location.definition.map(|line| line as i64), parent_id,
                    location.start_byte as i64, location.end_byte as i64, location.name_start as i64, location.name_end as i64,
                    location.is_test || file.path.starts_with("tests/") || file.path.contains("/tests/")])?;
            let location_id = tx.last_insert_rowid();
            location_ids.push(location_id);
            let name_text = text::searchable(location.symbol.as_deref().unwrap_or(""));
            let doc_text = text::searchable(&location.docs);
            // Independent source-derived views. Only combined rows enter FTS, preserving the lexical baseline.
            let first = location.regions.first().map_or(1, |r| r.start);
            if location.symbol.is_some() {
                let identity = format!(
                    "{}\n{}\n{}",
                    location.symbol.as_deref().unwrap_or(""),
                    file.path,
                    location.signature
                );
                insert_view(
                    &tx,
                    location_id,
                    first,
                    first,
                    &location.signature,
                    &identity,
                    "identity",
                )?;
            }
            if !location.docs.trim().is_empty() && location.kind != "documentation" {
                for region in
                    crate::extract::description_regions(&location.docs, location.docs_start)
                {
                    insert_view(
                        &tx,
                        location_id,
                        region.start,
                        region.end,
                        &region.source,
                        &region.source,
                        "description",
                    )?;
                }
            }
            for region in location.regions {
                let embedding_text = crate::embeddings::document(
                    location.symbol.as_deref(),
                    &file.path,
                    &location.docs,
                    &region.source,
                );
                tx.execute("INSERT INTO regions(location_id, start_line, end_line, source, embedding_text) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![location_id, region.start as i64, region.end as i64, region.source, embedding_text])?;
                tx.execute(
                    "INSERT INTO search(rowid, name, path, docs, body) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        tx.last_insert_rowid(),
                        name_text,
                        path_text,
                        doc_text,
                        text::searchable(&region.source)
                    ],
                )?;
                if location.kind != "file" || !has_declarations {
                    let view = if location.kind == "documentation" {
                        "description"
                    } else {
                        "implementation"
                    };
                    let implementation = if chunking == crate::Chunking::Ast
                        && view == "implementation"
                        && location.symbol.is_some()
                    {
                        // Context is encoder input only; the navigational span remains the exact body region.
                        format!(
                            "{}\n{}\n{}",
                            location.symbol.as_deref().unwrap_or(""),
                            location.signature.chars().take(384).collect::<String>(),
                            region.source
                        )
                    } else {
                        region.source.clone()
                    };
                    insert_view(
                        &tx,
                        location_id,
                        region.start,
                        region.end,
                        &region.source,
                        &implementation,
                        view,
                    )?;
                }
            }
        }
    }
    for (key, value) in [
        ("root", root_string),
        ("snapshot", snapshot.as_str()),
        ("extraction_version", extraction_version),
        (
            "resources",
            match resources {
                crate::ResourcePolicy::None => "none",
                crate::ResourcePolicy::Yaml => "yaml-v1",
            },
        ),
    ] {
        tx.execute("INSERT INTO metadata(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
    }
    let locations = tx.query_row("SELECT count(*) FROM locations", [], |row| {
        row.get::<_, u32>(0)
    })? as usize;
    let regions = tx.query_row(
        "SELECT count(*) FROM regions WHERE view='combined'",
        [],
        |row| row.get::<_, u32>(0),
    )? as usize;
    let parse_error_files = tx.query_row(
        "SELECT count(*) FROM files WHERE parse_errors != 0",
        [],
        |row| row.get::<_, u32>(0),
    )? as usize;
    tx.execute(
        "INSERT OR REPLACE INTO metadata VALUES ('languages',?1)",
        [crate::supported_languages().join(",")],
    )?;
    tx.commit()?;
    Ok(IndexReport {
        root: root_string.to_owned(),
        snapshot,
        files: files.len(),
        locations,
        regions,
        changed: changed.len(),
        removed: removed.len(),
        parse_error_files,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

#[allow(clippy::too_many_arguments)]
fn insert_view(
    conn: &Connection,
    location: i64,
    start: usize,
    end: usize,
    source: &str,
    text: &str,
    view: &str,
) -> Result<()> {
    conn.execute("INSERT INTO regions(location_id, start_line, end_line, source, embedding_text, view) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![location, start as i64, end as i64, source, text, view])?;
    Ok(())
}
