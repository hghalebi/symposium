//! Predicate *syntax*: the tree, its two surface spellings, and parsing.
//!
//! A predicate gates activation of a plugin or one of its components. This
//! module owns everything about what a predicate *is*: the [`Predicate`] tree,
//! parsing, `Display`, and serde, but nothing about evaluating one. Evaluation
//! needs the workspace dependency graph and the live environment, so it lives
//! in Symposium rather than here.
//!
//! That split is what lets a package manager parse and re-emit a manifest
//! without linking any of Symposium's evaluation machinery.
//!
//! Two surface syntaxes lower to the same tree:
//!
//! - The `depends-on` field uses **dependency-atom** syntax (`serde`,
//!   `serde>=1.0`, `*`) and lowers to `depends-on(...)` / `depends-on(*)`
//!   predicates, OR-combined into a single `any(...)` that is appended to the
//!   same predicate list.
//! - The `predicates` field uses **function-call** syntax:
//!   - `depends-on(<atom>)` — a workspace dependency is present (and its version
//!     satisfies the optional requirement); `depends-on(*)` matches any workspace.
//!   - `shell(<command>)` — `sh -c <command>` exits 0.
//!   - `path_exists(<arg>)` — `<arg>` exists on disk, falling back to a `$PATH`
//!     lookup for bare names.
//!   - `env(<name>)` / `env(<name>=<value>)` — env var presence / equality.
//!   - `workspace-member()`: the plugin this predicate belongs to is defined by
//!     a member of the active workspace.
//!   - `not(<p>)` — negation.
//!   - `any(<p>, …)` — OR.
//!   - `all(<p>, …)` — AND.
//!
//! Within a [`PredicateSet`] the entries are ANDed.

/// A predicate name, grammar, dependency atom, or version requirement is invalid.
///
/// Diagnostics identify the failed rule without echoing command, path, or
/// environment contents. Underlying version-parser errors remain inspectable.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PredicateSyntaxError {
    /// A custom predicate identifier is empty.
    #[error("predicate name is empty")]
    EmptyName,
    /// A custom identifier does not begin with an ASCII letter.
    #[error("predicate name must start with a letter")]
    NameMustStartWithLetter,
    /// A custom identifier contains a character outside its allowed alphabet.
    #[error(
        "predicate name contains an invalid character at position {position}; only ASCII alphanumeric and `_` are allowed"
    )]
    InvalidNameCharacter {
        /// The offending character's byte position in the identifier.
        position: usize,
        /// The offending character, retained for structured inspection.
        character: char,
    },
    /// A custom identifier collides with a builtin or retired builtin name.
    #[error("predicate name collides with a builtin predicate name")]
    ReservedName,
    /// Function-call syntax is required at this position.
    #[error("predicate is not a function call; expected `name(arg)`")]
    ExpectedFunctionCall,
    /// A function-call expression lacks its final closing parenthesis.
    #[error("predicate must end with `)`")]
    MissingClosingParen,
    /// The retired crate function must be replaced with depends-on.
    #[error("`crate(...)` is no longer supported; use `depends-on(...)` instead")]
    RetiredCrateSyntax,
    /// A builtin that takes no arguments received one.
    #[error("`{predicate}()` takes no argument")]
    UnexpectedArgument {
        /// The builtin predicate, never an input argument.
        predicate: &'static str,
    },
    /// An any/all combinator needs at least one child predicate.
    #[error("`{predicate}(...)` requires at least one predicate")]
    EmptyCombinator {
        /// The builtin combinator name.
        predicate: &'static str,
    },
    /// An environment predicate needs a variable name.
    #[error("`env(...)` requires a non-empty variable name")]
    EmptyEnvironmentName,
    /// A single dependency atom is empty.
    #[error("empty depends-on predicate")]
    EmptyDependency,
    /// A dependency atom does not begin with a valid dependency name.
    #[error("expected dependency name at position {position}")]
    ExpectedDependencyName {
        /// The byte position at which a name was required.
        position: usize,
    },
    /// Function calls belong in predicates rather than depends-on atom fields.
    #[error(
        "function-call syntax is not valid in the `depends-on` field; use the `predicates` field instead"
    )]
    FunctionCallInDependency,
    /// A dependency atom has unconsumed trailing input.
    #[error("unexpected trailing input at position {position}")]
    TrailingInput {
        /// The first unconsumed byte position.
        position: usize,
    },
    /// A dependency's version constraint is not valid SemVer syntax.
    #[error("invalid dependency version requirement")]
    VersionRequirement {
        /// The original version-parser failure.
        #[from]
        source: semver::Error,
    },
    /// One atom in a depends-on list failed to parse.
    #[error("failed to parse depends-on predicate")]
    DependencyAtom {
        /// The atom's specific syntax failure.
        #[source]
        source: Box<PredicateSyntaxError>,
    },
}

/// Names reserved for builtin predicates. Custom predicates must not use
/// these. `crate` is retired syntax but stays reserved so a custom predicate
/// can never squat on it.
pub const BUILTIN_PREDICATE_NAMES: &[&str] = &[
    "depends-on",
    "crate",
    "shell",
    "path_exists",
    "env",
    "workspace-member",
    "not",
    "any",
    "all",
];

/// A single predicate node.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// `depends-on(<name>)` / `depends-on(<name><req>)` — a workspace dep matches.
    DependsOn(String, Option<semver::VersionReq>),
    /// `depends-on(*)` / bare `*` — matches any workspace (even with zero deps).
    DependsOnWildcard,
    /// `shell(<command>)` — passes when `sh -c <command>` exits 0.
    Shell(String),
    /// `path_exists(<arg>)` — passes when `<arg>` exists (disk, then `$PATH`).
    PathExists(String),
    /// `env(<name>)` / `env(<name>=<value>)` — env var presence / equality.
    Env(String, Option<String>),
    /// `workspace-member()` — the plugin being evaluated is defined by a
    /// member of the active workspace (provenance, stamped by the loader).
    /// Selects content by audience: gate a component on it to activate only
    /// for people developing the defining package, not for dependents.
    WorkspaceMember,
    /// `not(<p>)` — passes when the inner predicate does not.
    Not(Box<Predicate>),
    /// `any(<p>, …)` — passes when at least one inner predicate does.
    Any(Vec<Predicate>),
    /// `all(<p>, …)` — passes when every inner predicate does.
    All(Vec<Predicate>),
    /// A plugin-defined predicate evaluated by spawning an external command.
    Custom { name: String, arg: String },
}

impl Predicate {
    /// Returns true if this predicate references the given dependency name
    /// anywhere (including inside combinators and negations).
    pub fn references_dep(&self, name: &str) -> bool {
        match self {
            Predicate::DependsOn(n, _) => n == name,
            Predicate::Not(p) => p.references_dep(name),
            Predicate::Any(v) | Predicate::All(v) => v.iter().any(|p| p.references_dep(name)),
            Predicate::DependsOnWildcard
            | Predicate::Shell(_)
            | Predicate::PathExists(_)
            | Predicate::Env(..)
            | Predicate::WorkspaceMember
            | Predicate::Custom { .. } => false,
        }
    }

    /// True if this predicate mentions any dependency (concrete or
    /// `depends-on(*)`).
    pub fn mentions_dep(&self) -> bool {
        match self {
            Predicate::DependsOn(..) | Predicate::DependsOnWildcard => true,
            Predicate::Not(p) => p.mentions_dep(),
            Predicate::Any(v) | Predicate::All(v) => v.iter().any(Predicate::mentions_dep),
            Predicate::Shell(_)
            | Predicate::PathExists(_)
            | Predicate::Env(..)
            | Predicate::WorkspaceMember
            | Predicate::Custom { .. } => false,
        }
    }

    /// True if this predicate names a *concrete* dependency
    /// (`depends-on(serde)`), as opposed to only `depends-on(*)`.
    /// Non-allocating — used on the hook hot path.
    pub fn has_concrete_dep(&self) -> bool {
        match self {
            Predicate::DependsOn(..) => true,
            Predicate::Not(p) => p.has_concrete_dep(),
            Predicate::Any(v) | Predicate::All(v) => v.iter().any(Predicate::has_concrete_dep),
            Predicate::DependsOnWildcard
            | Predicate::Shell(_)
            | Predicate::PathExists(_)
            | Predicate::Env(..)
            | Predicate::WorkspaceMember
            | Predicate::Custom { .. } => false,
        }
    }

    /// Collect every dependency name referenced anywhere in this predicate.
    ///
    /// Used for crates.io existence validation, so it ignores tree position
    /// (a dependency named under `not(...)` is still validated). Custom
    /// predicates are a no-op — their names are dynamic.
    pub fn collect_dep_names(&self, out: &mut std::collections::BTreeSet<String>) {
        match self {
            Predicate::DependsOn(name, _) => {
                out.insert(name.clone());
            }
            Predicate::Not(p) => p.collect_dep_names(out),
            Predicate::Any(v) | Predicate::All(v) => {
                for p in v {
                    p.collect_dep_names(out);
                }
            }
            Predicate::DependsOnWildcard
            | Predicate::Shell(_)
            | Predicate::PathExists(_)
            | Predicate::Env(..)
            | Predicate::WorkspaceMember
            | Predicate::Custom { .. } => {}
        }
    }
}

/// A list of predicates, ANDed together.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PredicateSet {
    pub predicates: Vec<Predicate>,
}

impl PredicateSet {
    /// Parse a comma-separated list of **function-call** predicate expressions.
    ///
    /// Compatibility adapter retained until SDK 0.2. Prefer [`Self::try_parse`].
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        Ok(Self::try_parse(input)?)
    }

    /// Parse a comma-separated list of function-call predicates.
    ///
    /// # Errors
    /// Returns the specific syntax failure without echoing argument contents.
    pub fn try_parse(input: &str) -> Result<Self, PredicateSyntaxError> {
        Ok(Self {
            predicates: try_parse_comma_separated(input)?,
        })
    }

    /// Build a set from **dependency-atom** syntax (the `depends-on` field),
    /// lowering the OR-combined atoms into a single `any(...)` predicate.
    /// Empty input yields an empty set.
    ///
    /// Compatibility adapter retained until SDK 0.2. Prefer
    /// [`Self::try_from_depends_on`].
    pub fn from_depends_on(input: &str) -> anyhow::Result<Self> {
        Ok(Self::try_from_depends_on(input)?)
    }

    /// Lower dependency atoms into an OR-combined predicate in this set.
    ///
    /// Empty input yields an empty set.
    ///
    /// # Errors
    /// Returns a typed syntax error for an invalid dependency atom.
    pub fn try_from_depends_on(input: &str) -> Result<Self, PredicateSyntaxError> {
        Ok(Self {
            predicates: DependsOnList::try_parse(input)?
                .into_predicate()
                .into_iter()
                .collect(),
        })
    }

    /// Combine a lowered `depends-on` field with a `predicates` field into one
    /// set. The `depends-on` atoms become a single leading `any(...)` predicate.
    pub fn merged(depends_on: Option<DependsOnList>, predicates: PredicateSet) -> PredicateSet {
        let mut list = Vec::new();
        if let Some(p) = depends_on.and_then(DependsOnList::into_predicate) {
            list.push(p);
        }
        list.extend(predicates.predicates);
        PredicateSet { predicates: list }
    }

    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty()
    }

    pub fn collect_dep_names(&self, out: &mut std::collections::BTreeSet<String>) {
        for p in &self.predicates {
            p.collect_dep_names(out);
        }
    }

    /// True if any `depends-on(...)` predicate (non-wildcard) appears anywhere.
    pub fn has_concrete_dep(&self) -> bool {
        self.predicates.iter().any(Predicate::has_concrete_dep)
    }

    /// True if any dependency predicate (including `depends-on(*)`) appears
    /// anywhere.
    pub fn mentions_dep(&self) -> bool {
        self.predicates.iter().any(Predicate::mentions_dep)
    }

    /// True if any predicate references the given dependency name.
    pub fn references_dep(&self, name: &str) -> bool {
        self.predicates.iter().any(|p| p.references_dep(name))
    }
}

// --- the `depends-on` field: a list of dependency atoms, OR-combined ---

/// The parsed `depends-on = [...]` field — a list of crate atoms. Lowers to a
/// single `any(...)` predicate appended to the enclosing predicate list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DependsOnList(pub Vec<Predicate>);

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum RawDependsOnList {
    One(String),
    Many(Vec<String>),
}

impl DependsOnList {
    /// Parse comma-separated dependency atoms (`serde, tokio>=1.0, *`).
    ///
    /// Commas inside balanced parentheses are preserved so that custom
    /// predicates like `battery_pack(a, b)` are not split incorrectly.
    /// Compatibility adapter retained until SDK 0.2. Prefer [`Self::try_parse`].
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        Ok(Self::try_parse(input)?)
    }

    /// Parse comma-separated dependency atoms, preserving nested comma groups.
    ///
    /// # Errors
    /// Returns [`PredicateSyntaxError::DependencyAtom`] with the specific atom
    /// failure attached, without including the raw atom in its diagnostic.
    pub fn try_parse(input: &str) -> Result<Self, PredicateSyntaxError> {
        let atoms = split_top_level(input)
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| {
                try_parse_dep_atom(s).map_err(|source| PredicateSyntaxError::DependencyAtom {
                    source: Box::new(source),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self(atoms))
    }

    /// Lower to a single predicate (`any(...)` over the atoms), or `None` if
    /// empty. A single atom is returned directly rather than wrapped.
    pub fn into_predicate(self) -> Option<Predicate> {
        match self.0.len() {
            0 => None,
            1 => self.0.into_iter().next(),
            _ => Some(Predicate::Any(self.0)),
        }
    }
}

impl serde::Serialize for DependsOnList {
    /// Serialized back to the atom spellings, so a manifest round-trips.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for p in &self.0 {
            seq.serialize_element(&dep_atom_string(p))?;
        }
        seq.end()
    }
}

/// Render a lowered `depends-on` atom back to its surface spelling. The atom
/// field accepts only `DependsOn` / `DependsOnWildcard`, so nothing else can
/// appear here; anything else falls back to the function-call form rather than
/// panicking.
fn dep_atom_string(p: &Predicate) -> String {
    match p {
        Predicate::DependsOn(name, None) => name.clone(),
        Predicate::DependsOn(name, Some(req)) => format!("{name}{req}"),
        Predicate::DependsOnWildcard => "*".to_string(),
        other => other.to_string(),
    }
}

impl<'de> serde::Deserialize<'de> for DependsOnList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Accept either a single string (`depends-on = "serde"`) or a sequence
        // (`depends-on = ["serde", "tokio>=1.0"]`).
        let atoms = match RawDependsOnList::deserialize(deserializer)? {
            RawDependsOnList::One(s) => vec![s],
            RawDependsOnList::Many(v) => v,
        };
        let predicates = atoms
            .iter()
            .map(|s| try_parse_dep_atom(s.trim()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom)?;
        Ok(Self(predicates))
    }
}

// --- function-call predicate parsing ---

/// Validate that `name` is a legal custom predicate identifier:
/// `[a-zA-Z][a-zA-Z0-9_]*`, must not collide with a builtin name.
///
/// Shared by both the expression parser (encountering an unknown function
/// name) and the `[[predicate]]` definition validator.
/// Compatibility adapter retained until SDK 0.2. Prefer
/// [`try_validate_custom_predicate_name`].
pub fn validate_custom_predicate_name(name: &str) -> anyhow::Result<()> {
    Ok(try_validate_custom_predicate_name(name)?)
}

/// Validate a custom identifier without including its contents in diagnostics.
///
/// # Errors
/// Returns the violated naming rule and any invalid character's byte position.
pub fn try_validate_custom_predicate_name(name: &str) -> Result<(), PredicateSyntaxError> {
    if name.is_empty() {
        return Err(PredicateSyntaxError::EmptyName);
    }
    if !name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        return Err(PredicateSyntaxError::NameMustStartWithLetter);
    }
    if let Some((position, character)) = name
        .char_indices()
        .find(|(_, character)| !character.is_ascii_alphanumeric() && *character != '_')
    {
        return Err(PredicateSyntaxError::InvalidNameCharacter {
            position,
            character,
        });
    }
    if BUILTIN_PREDICATE_NAMES.contains(&name) {
        return Err(PredicateSyntaxError::ReservedName);
    }
    Ok(())
}

/// Parse a single function-call predicate expression (`depends-on(serde)`,
/// `any(env(CI), shell(test -f x))`).
///
/// Compatibility adapter retained until SDK 0.2. Prefer [`try_parse_predicate`].
pub fn parse_predicate(input: &str) -> anyhow::Result<Predicate> {
    Ok(try_parse_predicate(input)?)
}

/// Parse one function-call expression into a predicate tree.
///
/// # Errors
/// Returns a specific naming, grammar, dependency, or version failure. Argument
/// contents are excluded from its diagnostic.
pub fn try_parse_predicate(input: &str) -> Result<Predicate, PredicateSyntaxError> {
    let trimmed = input.trim();
    let Some(open) = trimmed.find('(') else {
        return Err(PredicateSyntaxError::ExpectedFunctionCall);
    };
    if !trimmed.ends_with(')') {
        return Err(PredicateSyntaxError::MissingClosingParen);
    }
    let name = trimmed[..open].trim();
    // Everything between the first `(` and the final `)` is the argument; an
    // inner `)` (as in `shell(echo $(date))`) is preserved.
    let arg = trimmed[open + 1..trimmed.len() - 1].trim();

    match name {
        "depends-on" => try_parse_dep_atom(arg),
        "crate" => Err(PredicateSyntaxError::RetiredCrateSyntax),
        "shell" => Ok(Predicate::Shell(arg.to_string())),
        "path_exists" => Ok(Predicate::PathExists(arg.to_string())),
        "env" => parse_env(arg),
        "workspace-member" => {
            if !arg.is_empty() {
                return Err(PredicateSyntaxError::UnexpectedArgument {
                    predicate: "workspace-member",
                });
            }
            Ok(Predicate::WorkspaceMember)
        }
        "not" => Ok(Predicate::Not(Box::new(try_parse_predicate(arg)?))),
        "any" => {
            let preds = try_parse_comma_separated(arg)?;
            if preds.is_empty() {
                return Err(PredicateSyntaxError::EmptyCombinator { predicate: "any" });
            }
            Ok(Predicate::Any(preds))
        }
        "all" => {
            let preds = try_parse_comma_separated(arg)?;
            if preds.is_empty() {
                return Err(PredicateSyntaxError::EmptyCombinator { predicate: "all" });
            }
            Ok(Predicate::All(preds))
        }
        other => {
            try_validate_custom_predicate_name(other)?;
            Ok(Predicate::Custom {
                name: other.to_string(),
                arg: arg.to_string(),
            })
        }
    }
}

/// Parse a comma-separated list of function-call predicate expressions.
/// Commas inside parentheses are not separators.
///
/// Compatibility adapter retained until SDK 0.2. Prefer
/// [`try_parse_comma_separated`].
pub fn parse_comma_separated(input: &str) -> anyhow::Result<Vec<Predicate>> {
    Ok(try_parse_comma_separated(input)?)
}

/// Parse function-call predicates separated by top-level commas.
///
/// # Errors
/// Returns the first predicate's typed syntax failure.
pub fn try_parse_comma_separated(input: &str) -> Result<Vec<Predicate>, PredicateSyntaxError> {
    split_top_level(input)
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(try_parse_predicate)
        .collect()
}

fn parse_env(arg: &str) -> Result<Predicate, PredicateSyntaxError> {
    match arg.split_once('=') {
        Some((name, value)) => {
            let name = name.trim();
            if name.is_empty() {
                return Err(PredicateSyntaxError::EmptyEnvironmentName);
            }
            Ok(Predicate::Env(name.to_string(), Some(value.to_string())))
        }
        None => {
            if arg.is_empty() {
                return Err(PredicateSyntaxError::EmptyEnvironmentName);
            }
            Ok(Predicate::Env(arg.to_string(), None))
        }
    }
}

/// Split on top-level commas, ignoring commas nested inside `(...)`.
fn split_top_level(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut current = String::new();
    for c in input.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out
}

// --- dependency-atom parsing (`serde`, `serde>=1.0`, `*`) ---

/// Parse a single dependency atom into a `DependsOn` / `DependsOnWildcard`
/// predicate.
///
/// Compatibility adapter retained until SDK 0.2. Prefer [`try_parse_dep_atom`].
pub fn parse_dep_atom(input: &str) -> anyhow::Result<Predicate> {
    Ok(try_parse_dep_atom(input)?)
}

/// Parse a dependency name, optional version requirement, or wildcard atom.
///
/// # Errors
/// Returns a grammar or version requirement failure without echoing input.
pub fn try_parse_dep_atom(input: &str) -> Result<Predicate, PredicateSyntaxError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(PredicateSyntaxError::EmptyDependency);
    }
    if input == "*" {
        return Ok(Predicate::DependsOnWildcard);
    }
    let mut parser = AtomParser::new(input);
    let pred = parser.parse_atom()?;
    parser.skip_whitespace();
    if parser.pos < parser.input.len() {
        return Err(PredicateSyntaxError::TrailingInput {
            position: parser.pos,
        });
    }
    Ok(pred)
}

struct AtomParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> AtomParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len() && self.input.as_bytes()[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn parse_atom(&mut self) -> Result<Predicate, PredicateSyntaxError> {
        self.skip_whitespace();
        let start = self.pos;

        // Consume dependency name: [a-zA-Z0-9_-]+
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                self.pos += 1;
            } else {
                break;
            }
        }

        let name = &self.input[start..self.pos];
        if name.is_empty() {
            return Err(PredicateSyntaxError::ExpectedDependencyName { position: start });
        }

        // Function-call syntax is NOT valid in dependency-atom position. The
        // `depends-on` field accepts only bare names + optional version
        // constraints. Full predicate expressions (including custom
        // predicates) belong in the `predicates` field.
        if self.pos < self.input.len() && self.input.as_bytes()[self.pos] == b'(' {
            return Err(PredicateSyntaxError::FunctionCallInDependency);
        }

        // Version constraint (starts with >=, <=, >, <, =, ^, ~). Bare `=` is
        // treated as `^` (compatible), matching Cargo's default.
        let version_req = if self.pos < self.input.len() {
            let next = self.input.as_bytes()[self.pos];
            if matches!(next, b'>' | b'<' | b'=' | b'^' | b'~') {
                let vstart = self.pos;
                while self.pos < self.input.len() {
                    let c = self.input.as_bytes()[self.pos];
                    if c.is_ascii_whitespace() {
                        break;
                    }
                    self.pos += 1;
                }
                let raw = self.input[vstart..self.pos].trim();
                let constraint = if let Some(rest) = raw.strip_prefix("==") {
                    std::borrow::Cow::Owned(format!("={rest}"))
                } else if let Some(rest) = raw.strip_prefix('=') {
                    std::borrow::Cow::Owned(format!("^{rest}"))
                } else {
                    std::borrow::Cow::Borrowed(raw)
                };
                Some(semver::VersionReq::parse(&constraint)?)
            } else {
                None
            }
        } else {
            None
        };

        Ok(Predicate::DependsOn(name.to_string(), version_req))
    }
}

// --- serde + Display ---

impl serde::Serialize for Predicate {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Predicate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        try_parse_predicate(&s).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for PredicateSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.predicates.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for PredicateSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            predicates: Vec::<Predicate>::deserialize(deserializer)?,
        })
    }
}

impl std::fmt::Display for Predicate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Predicate::DependsOn(name, None) => write!(f, "depends-on({name})"),
            Predicate::DependsOn(name, Some(req)) => write!(f, "depends-on({name}{req})"),
            Predicate::DependsOnWildcard => write!(f, "depends-on(*)"),
            Predicate::Shell(cmd) => write!(f, "shell({cmd})"),
            Predicate::PathExists(arg) => write!(f, "path_exists({arg})"),
            Predicate::Env(name, None) => write!(f, "env({name})"),
            Predicate::Env(name, Some(value)) => write!(f, "env({name}={value})"),
            Predicate::WorkspaceMember => write!(f, "workspace-member()"),
            Predicate::Not(inner) => write!(f, "not({inner})"),
            Predicate::Any(preds) => write!(f, "any({})", join(preds)),
            Predicate::All(preds) => write!(f, "all({})", join(preds)),
            Predicate::Custom { name, arg } => write!(f, "{name}({arg})"),
        }
    }
}

fn join(preds: &[Predicate]) -> String {
    preds
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_round_trips_through_its_display_form() {
        for input in [
            "depends-on(serde)",
            "depends-on(serde>=1.0.0)",
            "depends-on(*)",
            "shell(which cargo-nextest)",
            "path_exists(build.rs)",
            "env(CI)",
            "env(MODE=release)",
            "workspace-member()",
            "not(depends-on(serde))",
            "any(depends-on(serde), depends-on(tokio))",
            "all(depends-on(serde), env(CI))",
            "my_pred(arg)",
        ] {
            let parsed = try_parse_predicate(input).unwrap();
            assert_eq!(parse_predicate(input).unwrap(), parsed);
            let redisplayed = parsed.to_string();
            assert_eq!(
                try_parse_predicate(&redisplayed).unwrap(),
                parsed,
                "{input} did not round-trip (rendered as {redisplayed})"
            );
        }
    }

    #[test]
    fn depends_on_list_round_trips_through_json() {
        let list: DependsOnList =
            serde_json::from_str(r#"["serde", "tokio>=1.0.0", "*"]"#).unwrap();
        let json = serde_json::to_string(&list).unwrap();
        let back: DependsOnList = serde_json::from_str(&json).unwrap();
        assert_eq!(back, list);
    }

    #[test]
    fn depends_on_accepts_a_bare_string() {
        let list: DependsOnList = serde_json::from_str(r#""serde""#).unwrap();
        assert_eq!(list.0, vec![Predicate::DependsOn("serde".into(), None)]);
    }

    #[test]
    fn canonical_parsers_report_structured_grammar_failures() {
        assert!(matches!(
            try_validate_custom_predicate_name(""),
            Err(PredicateSyntaxError::EmptyName)
        ));
        assert!(matches!(
            try_validate_custom_predicate_name("0custom"),
            Err(PredicateSyntaxError::NameMustStartWithLetter)
        ));
        assert!(matches!(
            try_validate_custom_predicate_name("custom_é"),
            Err(PredicateSyntaxError::InvalidNameCharacter {
                position: 7,
                character: 'é'
            })
        ));
        assert!(matches!(
            try_validate_custom_predicate_name("shell"),
            Err(PredicateSyntaxError::ReservedName)
        ));
        assert!(matches!(
            try_parse_predicate("shell"),
            Err(PredicateSyntaxError::ExpectedFunctionCall)
        ));
        assert!(matches!(
            try_parse_predicate("shell(true"),
            Err(PredicateSyntaxError::MissingClosingParen)
        ));
        assert!(matches!(
            try_parse_predicate("crate(serde)"),
            Err(PredicateSyntaxError::RetiredCrateSyntax)
        ));
        assert!(matches!(
            try_parse_predicate("workspace-member(unexpected)"),
            Err(PredicateSyntaxError::UnexpectedArgument {
                predicate: "workspace-member"
            })
        ));
        for predicate in ["any", "all"] {
            assert!(matches!(
                try_parse_predicate(&format!("{predicate}()")),
                Err(PredicateSyntaxError::EmptyCombinator { predicate: failed }) if failed == predicate
            ));
        }
        for input in ["env()", "env(=value)"] {
            assert!(matches!(
                try_parse_predicate(input),
                Err(PredicateSyntaxError::EmptyEnvironmentName)
            ));
        }
        assert!(matches!(
            try_parse_dep_atom(""),
            Err(PredicateSyntaxError::EmptyDependency)
        ));
        assert!(matches!(
            try_parse_dep_atom("@serde"),
            Err(PredicateSyntaxError::ExpectedDependencyName { position: 0 })
        ));
        assert!(matches!(
            try_parse_dep_atom("shell(true)"),
            Err(PredicateSyntaxError::FunctionCallInDependency)
        ));
        assert!(matches!(
            try_parse_dep_atom("serde trailing"),
            Err(PredicateSyntaxError::TrailingInput { position: 6 })
        ));
    }

    #[test]
    fn version_and_depends_on_list_failures_preserve_sources() {
        use std::error::Error as _;

        let error = try_parse_dep_atom("serde>=invalid").unwrap_err();
        assert!(matches!(
            error,
            PredicateSyntaxError::VersionRequirement { .. }
        ));
        assert!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<semver::Error>())
                .is_some()
        );

        let error = DependsOnList::try_parse("serde, tokio>=invalid").unwrap_err();
        assert!(matches!(error, PredicateSyntaxError::DependencyAtom { .. }));
        let atom_error = error
            .source()
            .unwrap()
            .downcast_ref::<Box<PredicateSyntaxError>>()
            .unwrap()
            .as_ref();
        assert!(matches!(
            atom_error,
            PredicateSyntaxError::VersionRequirement { .. }
        ));
        assert!(
            atom_error
                .source()
                .and_then(|source| source.downcast_ref::<semver::Error>())
                .is_some()
        );
        assert!(
            error
                .source()
                .unwrap()
                .source()
                .unwrap()
                .is::<semver::Error>()
        );

        let legacy = parse_dep_atom("serde>=invalid").unwrap_err();
        assert!(matches!(
            legacy.downcast_ref::<PredicateSyntaxError>(),
            Some(PredicateSyntaxError::VersionRequirement { .. })
        ));
    }

    #[test]
    fn syntax_diagnostics_exclude_raw_arguments_and_dependency_input() {
        const CANARY: &str = "SYNTHETIC_PREDICATE_SECRET_CANARY";
        for input in [
            CANARY.to_string(),
            format!("shell(echo {CANARY}"),
            format!("crate({CANARY})"),
            format!("workspace-member({CANARY})"),
            format!("env(={CANARY})"),
            format!("custom-name({CANARY})"),
        ] {
            let error = anyhow::Error::from(try_parse_predicate(&input).unwrap_err());
            assert!(!format!("{error:#}").contains(CANARY));
        }
        for input in [
            format!("serde {CANARY}"),
            format!("custom({CANARY})"),
            format!("serde>={CANARY}"),
        ] {
            let error = anyhow::Error::from(DependsOnList::try_parse(&input).unwrap_err());
            assert!(!format!("{error:#}").contains(CANARY));
        }
        let expression = serde_json::to_string(&format!("workspace-member({CANARY})")).unwrap();
        let error = serde_json::from_str::<Predicate>(&expression).unwrap_err();
        assert!(!error.to_string().contains(CANARY));
        let atoms = serde_json::to_string(&vec![format!("custom({CANARY})")]).unwrap();
        let error = serde_json::from_str::<DependsOnList>(&atoms).unwrap_err();
        assert!(!error.to_string().contains(CANARY));
    }

    #[test]
    fn legacy_parser_signatures_remain_available() {
        let _: fn(&str) -> anyhow::Result<PredicateSet> = PredicateSet::parse;
        let _: fn(&str) -> anyhow::Result<PredicateSet> = PredicateSet::from_depends_on;
        let _: fn(&str) -> anyhow::Result<DependsOnList> = DependsOnList::parse;
        let _: fn(&str) -> anyhow::Result<()> = validate_custom_predicate_name;
        let _: fn(&str) -> anyhow::Result<Predicate> = parse_predicate;
        let _: fn(&str) -> anyhow::Result<Vec<Predicate>> = parse_comma_separated;
        let _: fn(&str) -> anyhow::Result<Predicate> = parse_dep_atom;
        assert_eq!(
            PredicateSet::try_parse("").unwrap(),
            PredicateSet::default()
        );
        assert_eq!(
            PredicateSet::try_from_depends_on("").unwrap(),
            PredicateSet::default()
        );
    }

    #[test]
    fn dependency_introspection_covers_every_variant_and_nested_composition() {
        use std::collections::BTreeSet;

        let cases: Vec<(Predicate, &[&str], bool)> = vec![
            (Predicate::DependsOn("serde".into(), None), &["serde"], true),
            (Predicate::DependsOnWildcard, &[], true),
            (Predicate::Shell("true".into()), &[], false),
            (Predicate::PathExists("Cargo.toml".into()), &[], false),
            (Predicate::Env("CI".into(), None), &[], false),
            (Predicate::WorkspaceMember, &[], false),
            (
                Predicate::Custom {
                    name: "custom".into(),
                    arg: "serde".into(),
                },
                &[],
                false,
            ),
            (
                Predicate::Not(Box::new(Predicate::DependsOnWildcard)),
                &[],
                true,
            ),
            (
                Predicate::Any(vec![
                    Predicate::DependsOn("serde".into(), None),
                    Predicate::Env("CI".into(), None),
                ]),
                &["serde"],
                true,
            ),
            (
                Predicate::All(vec![
                    Predicate::Not(Box::new(Predicate::DependsOn("tokio".into(), None))),
                    Predicate::Any(vec![
                        Predicate::DependsOn("serde".into(), None),
                        Predicate::DependsOn("serde".into(), None),
                    ]),
                ]),
                &["serde", "tokio"],
                true,
            ),
        ];
        for (predicate, names, mentions) in cases {
            let expected: BTreeSet<_> = names.iter().map(|name| name.to_string()).collect();
            let nested = Predicate::All(vec![
                Predicate::Any(vec![Predicate::Not(Box::new(predicate.clone()))]),
                Predicate::PathExists("unrelated".into()),
            ]);
            for tree in [predicate, nested] {
                let mut found = BTreeSet::new();
                tree.collect_dep_names(&mut found);
                assert_eq!(found, expected, "{tree}");
                assert_eq!(tree.mentions_dep(), mentions, "{tree}");
                assert_eq!(tree.has_concrete_dep(), !names.is_empty(), "{tree}");
                for name in ["serde", "tokio", "absent"] {
                    assert_eq!(tree.references_dep(name), expected.contains(name), "{tree}");
                }
            }
        }
    }
}
