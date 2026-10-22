//! Source roles restrict retrieval eligibility, not the meaning of a relationship.
use std::collections::BTreeSet;

use anyhow::Result;
use rmcp::schemars;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    rmcp::schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceRole {
    Implementation,
    Test,
    Documentation,
}

impl SourceRole {
    fn matches(self, kind: &str, test: bool) -> bool {
        match self {
            Self::Test => test,
            Self::Documentation => !test && kind == "documentation",
            Self::Implementation => !test && kind != "documentation",
        }
    }
}

pub(crate) struct Filter {
    pub excluded: BTreeSet<i64>,
    pub excluded_json: String,
    test_paths: BTreeSet<String>,
}

impl Filter {
    pub fn new(
        conn: &Connection,
        role: Option<SourceRole>,
        fresh: bool,
        snapshot: &str,
    ) -> Result<Self> {
        let mut filter = Self {
            excluded: BTreeSet::new(),
            excluded_json: "[]".into(),
            test_paths: BTreeSet::new(),
        };
        let Some(role) = role else { return Ok(filter) };
        // Only current literal module proofs can refine syntax-extracted roles.
        if crate::navigation::current(conn, fresh, snapshot)? {
            filter.test_paths = conn
                .prepare("SELECT path FROM navigation_test_files")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
        }
        let mut stmt = conn.prepare(
            "SELECT l.id,l.kind,l.is_test,f.path FROM locations l JOIN files f ON f.id=l.file_id",
        )?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, String>(3)?,
            ))
        })? {
            let (id, kind, test, path) = row?;
            if !role.matches(&kind, test || filter.test_paths.contains(&path)) {
                filter.excluded.insert(id);
            }
        }
        filter.excluded_json = serde_json::to_string(&filter.excluded)?;
        Ok(filter)
    }

    pub fn correct_roles(&self, candidates: &mut [crate::candidate::Candidate]) {
        for candidate in candidates {
            if self.test_paths.contains(&candidate.path) {
                candidate.is_test = true;
            }
        }
    }
}
