//! Support for loading `.tmPreferences` metadata files. These files contain
//! information related to indentation rules, comment markers,
//! and other syntax-specific things.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::fs::File;
use std::io::BufReader;
use std::str::FromStr;

use lazycell::AtomicLazyCell;
use onig::{Regex, SearchOptions};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json;

use super::scope::{MatchPower, Scope};
use super::super::LoadingError;
use super::super::highlighting::settings::*;
use super::super::highlighting::ScopeSelectors;

type Dict = serde_json::Map<String, Settings>;

/// A String representation of a `ScopeSelectors` instance.
type SelectorString = String;

/// A simple regex pattern, used for checking indentation state.
#[derive(Debug)]
pub struct Pattern {
    pub regex_str: String,
    pub regex: AtomicLazyCell<Regex>,
}

/// A collection of all loaded metadata.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub scoped_metadata: Vec<MetadataSet>,
}

/// Metadata for a particular `ScopeSelector`.
#[derive(Debug, Clone, PartialEq)]
pub struct MetadataSet {
    /// The raw string representation of this selector. We keep this around
    /// for serialization; it's easier than trying to rebuild it from the
    /// parsed `ScopeSelectors`.
    pub selector_string: SelectorString,
    /// The scope selector to which this metadata applies
    pub selector: ScopeSelectors,
    /// The actual metadata.
    pub items: MetadataItems,
}

/// Items loaded from `.tmPreferences` metadata files, for a particular scope.
/// For more information, see [Metadata Files](http://docs.sublimetext.info/en/latest/reference/metadata.html)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MetadataItems {
    pub increase_indent_pattern: Option<Pattern>,
    pub decrease_indent_pattern: Option<Pattern>,
    pub bracket_indent_next_line_pattern: Option<Pattern>,
    pub disable_indent_next_line_pattern: Option<Pattern>,
    pub unindented_line_pattern: Option<Pattern>,
    pub indent_parens: Option<bool>,
    #[serde(default)]
    pub shell_variables: BTreeMap<String, String>,
    /// For convenience; this is the first value in `shell_variables`
    /// with a key beginning: `TM_COMMENT_START` that doesn't have a
    /// corresponding `TM_COMMENT_END`.
    pub line_comment: Option<String>,
    /// The first pair of `TM_COMMENT_START` and `TM_COMMENT_END` items in
    /// `shell_variables`, if they exist.
    pub block_comment: Option<(String, String)>,

    #[serde(default)]
    __allow_adding_fields_without_major_semver_bump: (),
}

/// A type that can be deserialized from a `.tmPreferences` file.
/// Since multiple files can refer to the same scope, we merge them while loading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RawMetadataEntry {
    path: PathBuf,
    scope: SelectorString,
    settings: Dict,
}

/// Convenience type for loading heterogeneous metadata.
#[derive(Debug, Default, Clone)]
pub(crate) struct LoadMetadata {
    loaded: Vec<RawMetadataEntry>,
}

// all of these are optional, but we don't want to deserialize if
// we don't have at least _one_ of them present
const KEYS_WE_USE: &[&str] = &[
    "increaseIndentPattern",
    "decreaseIndentPattern",
    "bracketIndentNextLinePattern",
    "disableIndentNextLinePattern",
    "unIndentedLinePattern",
    "indentParens",
    "shellVariables",
];

impl LoadMetadata {
    /// Adds the provided `RawMetadataEntry`. When creating the final `Metadata`
    /// object, all `RawMetadataEntry` items are sorted by path, and items that
    /// share a scope selector are merged; last writer wins.
    pub fn add_raw(&mut self, raw: RawMetadataEntry) {
        self.loaded.push(raw);
    }

    /// Generates a `MetadataSet` from a single file
    #[cfg(test)]
    pub(crate) fn quick_load(path: &str) -> Result<MetadataSet, LoadingError> {
        let mut loaded = Self::default();
        let raw = RawMetadataEntry::load(path)?;
        loaded.add_raw(raw);
        let mut metadata: Metadata = loaded.into();
        Ok(metadata.scoped_metadata.pop().unwrap())
    }
}

impl From<LoadMetadata> for Metadata {
    fn from(src: LoadMetadata) -> Metadata {
        let LoadMetadata { mut loaded } = src;
        loaded.sort_unstable_by(|a, b| a.path.cmp(&b.path));

        let mut scoped_metadata: BTreeMap<SelectorString, Dict> = BTreeMap::new();

        for RawMetadataEntry { scope, settings, path } in loaded {
            let scoped_settings = scoped_metadata.entry(scope.clone())
                .or_insert_with(|| {
                    let mut d = Dict::new();
                    d.insert("source_file_path".to_string(), path.to_string_lossy().into());
                    d
                });

            for (key, value) in settings {
                if !KEYS_WE_USE.contains(&key.as_str()) {
                    continue;
                }

                if key.as_str() == "shellVariables" {
                    append_vars(scoped_settings, value, &scope);
                } else {
                    scoped_settings.insert(key, value);
                }
            }
        }

        let scoped_metadata = scoped_metadata.into_iter()
            .map(|r|
                 MetadataSet::from_raw(r)
                     .map_err(|e| eprintln!("{}", e))
                 )
            .flatten()
            .collect();
        Metadata { scoped_metadata }
    }
}

fn append_vars(obj: &mut Dict, vars: Settings, scope: &str) {
    #[derive(Deserialize)]
    struct KeyPair { name: String, value: Settings }
    #[derive(Deserialize)]
    struct ShellVars(Vec<KeyPair>);

    let shell_vars = obj.entry(String::from("shellVariables"))
        .or_insert_with(|| Dict::new().into())
        .as_object_mut().unwrap();
    match serde_json::from_value::<ShellVars>(vars) {
	Ok(vars) => {
	    for KeyPair { name, value } in vars.0 {
		shell_vars.insert(name, value);
	    }
	}
	Err(e) => eprintln!("malformed shell variables for scope {}, {:}", scope, e),
    }
}

impl Metadata {
    /// For a given stack of scopes, returns a [`ScopedMetadata`] object
    /// which provides convenient access to metadata items which match the stack.
    pub fn metadata_for_scope(&self, scope: &[Scope]) -> ScopedMetadata {
        let mut metadata_matches = self.scoped_metadata
            .iter()
            .filter_map(|meta_set| {
                meta_set.selector.does_match(scope)
                    .map(|score| (score, meta_set))
            }).collect::<Vec<_>>();

        metadata_matches.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        ScopedMetadata { items: metadata_matches }
    }

    pub(crate) fn merged_with_raw(self, raw: LoadMetadata) -> Metadata {
        let Metadata { mut scoped_metadata } = self;
        let mut final_items: BTreeMap<String, MetadataSet> = scoped_metadata
            .drain(..)
            .map(|ms| (ms.selector_string.clone(), ms))
            .collect();
        let Metadata { scoped_metadata } = raw.into();
        for item in scoped_metadata {
            final_items.insert(item.selector_string.clone(), item);
        }

        let scoped_metadata: Vec<MetadataSet> = final_items.into_iter()
            .map(|(_k, v)| v)
            .collect();

        Metadata { scoped_metadata }
    }
}

impl MetadataSet {

    const COMMENT_KEYS: &'static [(&'static str, &'static str)] = &[
        ("TM_COMMENT_START", "TM_COMMENT_END"),
        ("TM_COMMENT_START_2", "TM_COMMENT_END_2"),
        ("TM_COMMENT_START_3", "TM_COMMENT_END_3"),
    ];

    pub fn from_raw(tuple: (SelectorString, Dict)) -> Result<MetadataSet, String> {
        let (selector_string, mut settings) = tuple;
        // we just use this for more useful debug messages
        let path = settings.remove("source_file_path").map(|v| v.to_string())
            .unwrap_or_else(|| String::from("(path missing)"));


       if !KEYS_WE_USE.iter().any(|key| settings.contains_key(*key)) {
           return Err(format!("skipping {}", path));
       }

       let line_comment = settings.get("shellVariables").and_then(|v| v.as_object())
           .and_then(MetadataSet::get_line_comment_marker);
       let block_comment = settings.get("shellVariables").and_then(|v| v.as_object())
           .and_then(MetadataSet::get_block_comment_markers);


        let mut items: MetadataItems = serde_json::from_value(settings.into())
            .map_err(|e| format!("{}: {:?}", path, e))?;
        items.line_comment = line_comment;
        items.block_comment = block_comment;

        let selector = ScopeSelectors::from_str(&selector_string)
            .map_err(|e| format!("{}, {:?}", path, e))?;
        Ok(MetadataSet { selector_string, selector, items })
    }

    fn get_line_comment_marker(vars: &Dict) -> Option<String> {
        MetadataSet::COMMENT_KEYS.iter()
            .find(|(b, e)| vars.contains_key(*b) && !vars.contains_key(*e))
            .and_then(|(b, _e)| vars.get(*b).map(|v| v.as_str().unwrap().to_string()))
    }

    fn get_block_comment_markers(vars: &Dict) -> Option<(String, String)> {
        MetadataSet::COMMENT_KEYS.iter()
            .find(|(b, e)| vars.contains_key(*b) && vars.contains_key(*e))
            .map(|(b, e)| {
                let b_str = vars.get(*b).unwrap().as_str().unwrap().to_string();
                let e_str = vars.get(*e).unwrap().as_str().unwrap().to_string();
                (b_str, e_str)
            })
    }
}

/// A collection of `MetadataSet`s which match a given scope selector,
/// sorted in order of the strength of the match.
///
/// # Examples
///
/// ```
/// # #[macro_use] extern crate serde_json;
/// # extern crate syntect;
/// # use syntect::parsing::*;
/// # use syntect::highlighting::ScopeSelectors;
/// # use std::str::FromStr;
/// #
/// // given the following two scoped metadata collections:
///
/// let one_selector = "source.my_lang";
/// let one_items = json!({
///     "increaseIndentPattern": "one increase",
///     "decreaseIndentPattern": "one decrease",
/// });
/// # let one_items = one_items.as_object().cloned().unwrap();
///
/// let two_selector = "other.thing";
/// let two_items = json!({
///     "increaseIndentPattern": "two increase",
/// });
/// # let two_items = two_items.as_object().cloned().unwrap();
/// #
/// # let one = MetadataSet::from_raw((one_selector.into(), one_items)).unwrap();
/// # let two = MetadataSet::from_raw((two_selector.into(), two_items)).unwrap();
/// #
/// # let scoped_metadata = vec![one, two];
/// # let metadata = Metadata { scoped_metadata };
///
/// // both of which match this scope stack:
///
/// let query_scope = ScopeStack::from_str("source.my_lang other.thing").unwrap();
/// let scoped = metadata.metadata_for_scope(query_scope.as_slice());
///
/// // the better match is used when it has a field,
/// assert!(scoped.increase_indent("two increase"));
///
/// // and the other match is used when it does not.
/// assert!(scoped.decrease_indent("one decrease"));
/// ```
#[derive(Debug, Clone)]
pub struct ScopedMetadata<'a> {
    pub items: Vec<(MatchPower, &'a MetadataSet)>,
}

impl<'a> ScopedMetadata<'a> {

    pub fn unindented_line(&self, line: &str) -> bool {
        self.best_match(|ind| ind.unindented_line_pattern.as_ref().map(|p| p.is_match(line)))
            .unwrap_or(false)
    }

    pub fn decrease_indent(&self, line: &str) -> bool {
        self.best_match(|ind| ind.decrease_indent_pattern.as_ref().map(|p| p.is_match(line)))
            .unwrap_or(false)
    }

    pub fn increase_indent(&self, line: &str) -> bool {
        self.best_match(|ind| ind.increase_indent_pattern.as_ref().map(|p| p.is_match(line)))
            .unwrap_or(false)
    }

    pub fn bracket_increase(&self, line: &str) -> bool {
        self.best_match(|ind| ind.bracket_indent_next_line_pattern.as_ref().map(|p| p.is_match(line)))
            .unwrap_or(false)
    }

    pub fn disable_indent_next_line(&self, line: &str) -> bool {
        self.best_match(|ind| ind.disable_indent_next_line_pattern.as_ref().map(|p| p.is_match(line)))
            .unwrap_or(false)
    }

    pub fn line_comment(&self) -> Option<&str> {
        let idx = self.items.iter().position(|m| m.1.items.line_comment.is_some())?;
        self.items[idx].1.items.line_comment.as_ref().map(|s| s.as_str())
    }

    pub fn block_comment(&self) -> Option<(&str, &str)> {
        let idx = self.items.iter().position(|m| m.1.items.block_comment.is_some())?;
        self.items[idx].1.items.block_comment.as_ref().map(|(a, b)| (a.as_str(), b.as_str()))
    }

    fn best_match<T, F>(&self, f: F) -> Option<T>
        where F: FnMut(&MetadataItems) -> Option<T>
    {
        self.items.iter()
            .map(|(_, meta_set)| &meta_set.items)
            .flat_map(f)
            .next()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl RawMetadataEntry {
    pub fn load<P: Into<PathBuf>>(path: P) -> Result<Self, LoadingError> {
        let path: PathBuf = path.into();
        let file = File::open(&path)?;
        let file = BufReader::new(file);
        let mut contents = read_plist(file)?;
        // we stash the path because we use it to determine parse order
        // when generating the final metadata object; to_string_lossy
        // is adequate for this purpose.
        contents.as_object_mut().and_then(|obj| obj.insert("path".into(), path.to_string_lossy().into()));
        Ok(serde_json::from_value(contents)?)
    }
}

impl Pattern {
    pub fn is_match<S: AsRef<str>>(&self, string: S) -> bool {
        self.regex()
            .match_with_options(
                string.as_ref(),
                0,
                SearchOptions::SEARCH_OPTION_NONE,
                None)
            .is_some()
    }

    pub fn regex(&self) -> &Regex {
        if let Some(regex) = self.regex.borrow() {
            regex
        } else {
            let regex = Regex::new(&self.regex_str)
                .expect("regex string should be pre-tested");
            self.regex.fill(regex).ok();
            self.regex.borrow().unwrap()
        }
    }
}

impl Clone for Pattern {
    fn clone(&self) -> Self {
        Pattern { regex_str: self.regex_str.clone(), regex: AtomicLazyCell::new() }
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Pattern) -> bool {
        self.regex_str == other.regex_str
    }
}

impl Eq for Pattern {}

impl Serialize for Pattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> where S: Serializer {
        serializer.serialize_str(&self.regex_str)
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: Deserializer<'de> {
        let regex_str = String::deserialize(deserializer)?;
        Ok(Pattern { regex_str, regex: AtomicLazyCell::new() })
    }
}

#[derive(Serialize, Deserialize)]
struct MetaSetSerializable {
    selector_string: String,
    items: Option<MetadataItems>,
}

impl Serialize for MetadataSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> where S: Serializer {

        let MetadataSet { selector_string, items, .. } = self.clone();
        let inner = MetaSetSerializable { selector_string, items: Some(items) };
        inner.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MetadataSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: Deserializer<'de> {
        use serde::de::Error;
        let inner = MetaSetSerializable::deserialize(deserializer)?;
        let MetaSetSerializable { selector_string, items } = inner;
        let selector = ScopeSelectors::from_str(&selector_string)
            .map_err(|e| Error::custom(format!("{:?}", e)))?;
        let items = items.ok_or_else(|| Error::custom(format!("no metadata items")))?;
        Ok(MetadataSet { selector_string, selector, items })
    }
}


#[cfg(test)]
mod tests {
    use std::path::Path;
    use super::*;
    use parsing::SyntaxSet;

    #[test]
    fn load_raw() {
        let comments_file: &str = "testdata/Packages/Go/Comments.tmPreferences";
        assert!(Path::new(comments_file).exists());

        let r = RawMetadataEntry::load(comments_file);
        assert!(r.is_ok());

        let indent_file: &str = "testdata/Packages/Go/Indentation Rules.tmPreferences";
        assert!(Path::new(indent_file).exists());

        let r = RawMetadataEntry::load(indent_file).unwrap();
        assert_eq!(r.scope, "source.go");

        let indent_file: &str = "testdata/Packages/Rust/RustIndent.tmPreferences";
        assert!(Path::new(indent_file).exists());

        let r = RawMetadataEntry::load(indent_file).unwrap();
        assert_eq!(r.scope, "source.rust");
    }

    #[test]
    fn load_groups() {
        let mut loaded = LoadMetadata::default();
        let indent_file: &str = "testdata/Packages/Rust/RustIndent.tmPreferences";
        let raw = RawMetadataEntry::load(indent_file).expect("failed to load indent metadata");
        loaded.add_raw(raw);
        let comment_file: &str = "testdata/Packages/Rust/RustComment.tmPreferences";
        let raw = RawMetadataEntry::load(comment_file).expect("failed to load comment metadata");
        loaded.add_raw(raw);

        let metadata: Metadata = loaded.into();
        assert_eq!(metadata.scoped_metadata.len(), 1);

        let rust_meta = metadata.scoped_metadata.first().unwrap();
        assert!(rust_meta.selector_string == "source.rust");
        assert!(rust_meta.items.increase_indent_pattern.is_some());
        assert!(rust_meta.items.line_comment.is_some());
    }

    #[test]
    fn parse_yaml_meta() {
        let path = "testdata/Packages/YAML/Indentation Rules.tmPreferences";
        let metaset = LoadMetadata::quick_load(path).unwrap();
        assert!(metaset.items.increase_indent_pattern.is_some());
        assert!(metaset.items.decrease_indent_pattern.is_some());
        assert!(metaset.items.bracket_indent_next_line_pattern.is_none());
    }

    #[test]
    fn load_shell_vars() {
        let path = "testdata/Packages/AppleScript/Comments.tmPreferences";
        let metadata = LoadMetadata::quick_load(path).unwrap();
        assert!(metadata.items.shell_variables.get("TM_COMMENT_START").is_some());
        assert!(metadata.items.shell_variables.get("TM_COMMENT_END").is_none());
        assert!(metadata.items.shell_variables.get("TM_COMMENT_START_2").is_some());
        assert!(metadata.items.shell_variables.get("TM_COMMENT_START_3").is_some());
        assert!(metadata.items.shell_variables.get("TM_COMMENT_END_3").is_some());
        assert!(metadata.items.shell_variables.get("TM_COMMENT_DISABLE_INDENT_3").is_some());
        assert!(metadata.items.line_comment.is_some());
        assert!(metadata.items.block_comment.is_some());
        assert!(metadata.items.increase_indent_pattern.is_none());
    }

    #[test]
    fn serde_pattern() {
        let pattern: Pattern = serde_json::from_str("\"just a string\"").unwrap();
        assert_eq!(pattern.regex_str, "just a string");
        let back_to_str = serde_json::to_string(&pattern).unwrap();
        assert_eq!(back_to_str, "\"just a string\"");
    }

    #[test]
    fn indent_rust() {
        let ps = SyntaxSet::load_from_folder("testdata/Packages/Rust").unwrap();

        let rust_scopes = [Scope::new("source.rust").unwrap()];
        let indent_ctx = ps.metadata.metadata_for_scope(&rust_scopes);

        assert_eq!(indent_ctx.items.len(), 1, "failed to load rust metadata");
        assert_eq!(indent_ctx.increase_indent("struct This {"), true);
        assert_eq!(indent_ctx.increase_indent("struct This }"), false);
        assert_eq!(indent_ctx.decrease_indent("     }"), true);
        assert_eq!(indent_ctx.decrease_indent("struct This {"), false);
        assert_eq!(indent_ctx.decrease_indent("struct This {}"), false);
        assert_eq!(indent_ctx.increase_indent("struct This {}"), false);

    }
}
