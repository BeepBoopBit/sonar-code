use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Empty {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Brief {
    /// Restrict starting locations by source role; combine implementation with kind=function for code owners.
    pub role: Option<crate::SourceRole>,
    /// Task question, exact symbol, or indexed path; at most 16 KiB of UTF-8.
    #[schemars(length(min = 1, max = 16384))]
    pub query: String,
    /// Literal repository-relative file or directory, applied to all returned locations.
    pub scope: Option<String>,
    /// Literal repository-relative files/directories to omit before retrieval limits.
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub exclude: Vec<String>,
    /// Indexed kind for starting locations, for example function, field, or documentation.
    pub kind: Option<String>,
    /// Starting locations: 1–20, default 5.
    #[schemars(range(min = 1, max = 20))]
    pub limit: Option<usize>,
    /// Supporting locations: 0–10, default 3.
    #[schemars(range(min = 0, max = 10))]
    pub related: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Investigate {
    /// Task question; at most 16 KiB of UTF-8.
    #[schemars(length(min = 1, max = 16384))]
    pub query: String,
    /// A returned location's full handle. Omit for discovery; provide for evidenced context.
    #[schemars(length(max = 256))]
    pub from: Option<String>,
    /// Literal repository-relative file/directory; constrains every route location.
    pub scope: Option<String>,
    /// Literal repository-relative files/directories to omit, including route evidence.
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub exclude: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Locate {
    /// Repository-relative indexed source path, not an absolute filesystem path.
    pub path: String,
    /// One-based source line.
    #[schemars(range(min = 1))]
    pub line: usize,
    /// Optional one-based UTF-8 byte column, not a character offset.
    #[schemars(range(min = 1))]
    pub column: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Outline {
    /// Repository-relative indexed file.
    pub path: String,
    /// Locations per page: 1–100, default 50.
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<usize>,
    /// next_cursor returned by the preceding outline request for this file.
    #[schemars(length(max = 128))]
    pub cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Direction {
    Incoming,
    Outgoing,
    #[default]
    Both,
}
impl From<Direction> for crate::Direction {
    fn from(value: Direction) -> Self {
        match value {
            Direction::Incoming => Self::Incoming,
            Direction::Outgoing => Self::Outgoing,
            Direction::Both => Self::Both,
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Relation {
    #[default]
    All,
    Contains,
    Calls,
    References,
    Embeds,
    Documents,
}
impl From<Relation> for crate::Relation {
    fn from(value: Relation) -> Self {
        match value {
            Relation::All => Self::All,
            Relation::Contains => Self::Contains,
            Relation::Calls => Self::Calls,
            Relation::References => Self::References,
            Relation::Embeds => Self::Embeds,
            Relation::Documents => Self::Documents,
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum FollowRelation {
    #[default]
    Semantic,
    Calls,
    References,
}
impl From<FollowRelation> for crate::FollowRelation {
    fn from(value: FollowRelation) -> Self {
        match value {
            FollowRelation::Semantic => Self::Semantic,
            FollowRelation::Calls => Self::Calls,
            FollowRelation::References => Self::References,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Neighbors {
    /// Full snapshot-bound handle from a Sonar location, not a response-local ID.
    #[schemars(length(max = 256))]
    pub handle: String,
    /// Recorded relation to inspect; default all.
    #[serde(default)]
    pub relation: Relation,
    /// Recorded edge direction from the anchor; default both.
    #[serde(default)]
    pub direction: Direction,
    /// Literal repository-relative scope for both endpoints.
    pub scope: Option<String>,
    /// Connections per page: 1–100, default 20.
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<usize>,
    /// next_cursor from the same anchor/relation/direction/scope request.
    #[schemars(length(max = 128))]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Follow {
    /// Full snapshot-bound handle from a Sonar location.
    #[schemars(length(max = 256))]
    pub handle: String,
    /// Behavior or implementation to find in the recorded neighborhood.
    #[schemars(length(min = 1, max = 16384))]
    pub query: String,
    /// Calls and references by default; containment is not included.
    #[serde(default)]
    pub relation: FollowRelation,
    #[serde(default)]
    pub direction: Direction,
    /// Literal repository-relative scope for every route location.
    pub scope: Option<String>,
    /// Endpoint kind; intermediate kinds remain available to explain the route.
    pub kind: Option<String>,
    /// Maximum route depth: 1–4, default 2.
    #[schemars(range(min = 1, max = 4))]
    pub hops: Option<usize>,
    /// Matching endpoints: 1–20, default 5.
    #[schemars(range(min = 1, max = 20))]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct FieldUses {
    /// Full handle for a field or a function whose field uses should be inspected.
    #[schemars(length(max = 256))]
    pub handle: String,
    /// Restricts use-site files; referenced declarations can be outside this scope.
    pub scope: Option<String>,
    /// Use occurrences per page: 1–100, default 20.
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<usize>,
    /// next_cursor from the preceding request with the same anchor and scope.
    #[schemars(length(max = 128))]
    pub cursor: Option<String>,
}
