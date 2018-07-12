//! PII stripping and normalization rule configuration.

use std::cmp;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use hmac::{Hmac, Mac};
use regex::{Regex, RegexBuilder};
use serde::de::{Deserialize, Deserializer, Error};
use serde::ser::{Serialize, Serializer};
use sha1::Sha1;
use sha2::{Sha256, Sha512};

use chunk::{self, Chunk};
use common::Value;
use detectors;
use meta::{Annotated, Meta, Note, Remark};
use processor::{PiiKind, PiiProcessor, ProcessAnnotatedValue, ValueInfo};

lazy_static! {
    static ref NULL_SPLIT_RE: Regex = Regex::new("\x00").unwrap();
}

/// Indicates that the rule config was invalid after parsing.
#[derive(Fail, Debug)]
pub enum BadRuleConfig {
    /// An invalid reference to a rule was found in the config.
    #[fail(display = "invalid rule reference ({})", _0)]
    BadReference(String),
}

/// A regex pattern for text replacement.
pub struct Pattern(pub Regex);

impl fmt::Debug for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl Serialize for Pattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let pattern = RegexBuilder::new(&raw)
            .size_limit(262_144)
            .build()
            .map_err(Error::custom)?;
        Ok(Pattern(pattern))
    }
}

/// Supported stripping rules.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum RuleType {
    /// Applies a regular expression.
    #[serde(rename_all = "camelCase")]
    Pattern {
        /// The regular expression to apply.
        pattern: Pattern,
        /// The match group indices to replace.
        replace_groups: Option<BTreeSet<u8>>,
    },
    /// Matches an email
    Email,
    /// Matches an IPv4 address
    Ipv4,
    /// Matches an IPv6 address
    Ipv6,
    /// Matches any IP address
    Ip,
    /// Matches a creditcard number
    Creditcard,
    /// Unconditionally removes the value
    Remove,
    /// When a regex matches a key, a value is removed
    #[serde(rename_all = "camelCase")]
    RemovePair {
        /// A pattern to match for keys.
        key_pattern: Pattern,
    },
}

/// Defines the hash algorithm to use for hashing
#[derive(Serialize, Deserialize, Debug)]
pub enum HashAlgorithm {
    /// HMAC-SHA1
    #[serde(rename = "HMAC-SHA1")]
    HmacSha1,
    /// HMAC-SHA256
    #[serde(rename = "HMAC-SHA256")]
    HmacSha256,
    /// HMAC-SHA512
    #[serde(rename = "HMAC-SHA512")]
    HmacSha512,
}

impl Default for HashAlgorithm {
    fn default() -> HashAlgorithm {
        HashAlgorithm::HmacSha256
    }
}

impl HashAlgorithm {
    fn hash_value(&self, text: &str, key: &str) -> String {
        macro_rules! hmac {
            ($ty:ident) => {{
                let mut mac = Hmac::<$ty>::new_varkey(key.as_bytes()).unwrap();
                mac.input(text.as_bytes());
                format!("{:X}", mac.result().code())
            }};
        }
        match *self {
            HashAlgorithm::HmacSha1 => hmac!(Sha1),
            HashAlgorithm::HmacSha256 => hmac!(Sha256),
            HashAlgorithm::HmacSha512 => hmac!(Sha512),
        }
    }
}

fn default_mask_char() -> char {
    '*'
}

/// Defines how replacements happen.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "method", rename_all = "camelCase")]
pub(crate) enum Redaction {
    /// Replaces the matched group with a new value.
    #[serde(rename_all = "camelCase")]
    Replace {
        /// The replacement string.
        new_value: Value,
    },
    /// Overwrites the matched value by masking.
    #[serde(rename_all = "camelCase")]
    Mask {
        /// The character to mask with.
        #[serde(default = "default_mask_char")]
        mask_char: char,
        /// Characters to skip during masking to preserve structure.
        #[serde(default)]
        chars_to_ignore: String,
        /// Index range to mask in. Negative indices count from the string's end.
        #[serde(default)]
        range: (Option<i32>, Option<i32>),
    },
    /// Replaces the value with a hash
    #[serde(rename_all = "camelCase")]
    Hash {
        /// The hash algorithm
        #[serde(default)]
        algorithm: HashAlgorithm,
        /// The secret key
        key: String,
    },
}

fn in_range(range: (Option<i32>, Option<i32>), pos: usize, len: usize) -> bool {
    fn get_range_index(idx: Option<i32>, len: usize, default: usize) -> usize {
        match idx {
            None => default,
            Some(idx) if idx < 0 => len.saturating_sub((idx * -1) as usize),
            Some(idx) => cmp::min(idx as usize, len),
        }
    }

    let start = get_range_index(range.0, len, 0);
    let end = get_range_index(range.1, len, len);
    pos >= start && pos < end
}

impl Redaction {
    fn insert_replacement_chunks(&self, text: &str, note: Note, output: &mut Vec<Chunk>) {
        match *self {
            Redaction::Mask {
                mask_char,
                ref chars_to_ignore,
                range,
            } => {
                let chars_to_ignore: BTreeSet<char> = chars_to_ignore.chars().collect();
                let mut buf = Vec::with_capacity(text.len());

                for (idx, c) in text.chars().enumerate() {
                    if in_range(range, idx, text.len()) && !chars_to_ignore.contains(&c) {
                        buf.push(mask_char);
                    } else {
                        buf.push(c);
                    }
                }
                output.push(Chunk::Redaction(buf.into_iter().collect(), note));
            }
            Redaction::Hash {
                ref algorithm,
                ref key,
            } => {
                output.push(Chunk::Redaction(
                    algorithm.hash_value(text, key.as_str()),
                    note,
                ));
            }
            Redaction::Replace { ref new_value } => {
                output.push(Chunk::Redaction(new_value.to_string().into(), note));
            }
        }
    }

    fn set_replacement_value(
        &self,
        mut annotated: Annotated<Value>,
        note: Note,
    ) -> Annotated<Value> {
        match *self {
            Redaction::Mask { .. } => match annotated {
                Annotated(Some(value), meta) => {
                    let value_as_string = value.to_string();
                    let original_length = value_as_string.len();
                    let mut output = vec![];
                    self.insert_replacement_chunks(&value_as_string, note, &mut output);
                    let (value, mut meta) = chunk::chunks_to_string(output, meta);
                    if value.len() != original_length && meta.original_length.is_none() {
                        meta.original_length = Some(original_length as u32);
                    }
                    Annotated(Some(Value::String(value)), meta)
                }
                annotated @ Annotated(None, _) => annotated.with_removed_value(Remark::new(note)),
            },
            Redaction::Hash {
                ref algorithm,
                ref key,
            } => match annotated {
                Annotated(Some(value), mut meta) => {
                    let value_as_string = value.to_string();
                    let original_length = value_as_string.len();
                    let value = algorithm.hash_value(&value_as_string, key.as_str());
                    if value.len() != original_length && meta.original_length.is_none() {
                        meta.original_length = Some(original_length as u32);
                    }
                    Annotated(Some(Value::String(value)), meta)
                }
                annotated @ Annotated(None, _) => annotated.with_removed_value(Remark::new(note)),
            },
            Redaction::Replace { ref new_value } => {
                annotated.set_value(Some(new_value.clone()));
                annotated.meta_mut().remarks_mut().push(Remark::new(note));
                annotated
            }
        }
    }
}

/// A single rule configuration.
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct RuleSpec {
    #[serde(flatten)]
    ty: RuleType,
    redaction: Option<Redaction>,
    note: Option<String>,
}

/// A rule is a rule config plus id.
#[derive(Debug)]
pub(crate) struct Rule<'a> {
    id: &'a str,
    spec: &'a RuleSpec,
}

/// A set of named rule configurations.
#[derive(Serialize, Deserialize, Debug)]
pub struct RuleConfig {
    rules: BTreeMap<String, RuleSpec>,
    #[serde(default)]
    applications: BTreeMap<PiiKind, Vec<String>>,
}

/// A PII processor that uses JSON rules.
pub struct RuleBasedPiiProcessor<'a> {
    cfg: &'a RuleConfig,
    applications: BTreeMap<PiiKind, Vec<Rule<'a>>>,
}

impl<'a> Rule<'a> {
    /// Creates a new note.
    pub fn create_note(&self) -> Note {
        Note::new(self.id.to_string(), self.spec.note.clone())
    }

    /// Inserts replacement chunks into the given chunk buffer.
    ///
    /// If the rule is configured with `redaction` then replacement chunks are
    /// added to the buffer based on that information.  If `redaction` is not
    /// defined an empty redaction chunk is added with the supplied note.
    fn insert_replacement_chunks(&self, text: &str, output: &mut Vec<Chunk>) {
        let note = self.create_note();
        if let Some(ref redaction) = self.spec.redaction {
            redaction.insert_replacement_chunks(text, note, output);
        } else {
            output.push(Chunk::Redaction("".to_string(), note));
        }
    }

    /// Produces a new annotated value with replacement data.
    ///
    /// This fully replaces the value in the annotated value with the replacement value
    /// from the config.  If no replacement value is defined (which is likely) then
    /// then no value is set (null).  In either case the given note is recorded.
    fn replace_value(&self, annotated: Annotated<Value>) -> Annotated<Value> {
        let note = self.create_note();
        if let Some(ref redaction) = self.spec.redaction {
            redaction.set_replacement_value(annotated, note)
        } else {
            annotated.with_removed_value(Remark::new(note))
        }
    }

    /// Processes the given chunks according to the rule.
    ///
    /// This works the same as `pii_process_chunks` in behavior.  This means that if an
    /// error is returned the caller falls back to regular value processing.
    fn process_chunks(
        &self,
        chunks: Vec<Chunk>,
        meta: Meta,
    ) -> Result<(Vec<Chunk>, Meta), (Vec<Chunk>, Meta)> {
        match self.spec.ty {
            RuleType::Pattern {
                ref pattern,
                ref replace_groups,
            } => Ok(self.apply_regex_to_chunks(chunks, meta, &pattern.0, replace_groups.as_ref())),
            RuleType::Email => {
                Ok(self.apply_regex_to_chunks(chunks, meta, &*detectors::EMAIL_REGEX, None))
            }
            RuleType::Ipv4 => {
                Ok(self.apply_regex_to_chunks(chunks, meta, &*detectors::IPV4_REGEX, None))
            }
            RuleType::Ipv6 => {
                Ok(self.apply_regex_to_chunks(chunks, meta, &*detectors::IPV6_REGEX, None))
            }
            RuleType::Ip => {
                let (chunks, meta) =
                    self.apply_regex_to_chunks(chunks, meta, &*detectors::IPV4_REGEX, None);
                let (chunks, meta) =
                    self.apply_regex_to_chunks(chunks, meta, &*detectors::IPV6_REGEX, None);
                Ok((chunks, meta))
            }
            RuleType::Creditcard => {
                Ok(self.apply_regex_to_chunks(chunks, meta, &*detectors::CREDITCARD_REGEX, None))
            }
            // no special handling for strings, falls back to `process_value`
            RuleType::Remove | RuleType::RemovePair { .. } => Err((chunks, meta)),
        }
    }

    /// Applies a regex to chunks and meta.
    fn apply_regex_to_chunks(
        &self,
        chunks: Vec<Chunk>,
        meta: Meta,
        regex: &Regex,
        replace_groups: Option<&BTreeSet<u8>>,
    ) -> (Vec<Chunk>, Meta) {
        let mut search_string = String::new();
        let mut replacement_chunks = vec![];
        for chunk in chunks {
            match chunk {
                Chunk::Text(ref text) => search_string.push_str(&text.replace("\x00", "")),
                chunk @ Chunk::Redaction(..) => {
                    replacement_chunks.push(chunk);
                    search_string.push('\x00');
                }
            }
        }
        replacement_chunks.reverse();
        let mut rv: Vec<Chunk> = vec![];

        fn process_text(text: &str, rv: &mut Vec<Chunk>, replacement_chunks: &mut Vec<Chunk>) {
            if text.is_empty() {
                return;
            }
            let mut pos = 0;
            for piece in NULL_SPLIT_RE.find_iter(text) {
                rv.push(Chunk::Text(text[pos..piece.start()].to_string().into()));
                rv.push(replacement_chunks.pop().unwrap());
                pos = piece.end();
            }
            rv.push(Chunk::Text(text[pos..].to_string().into()));
        }

        let mut pos = 0;
        for m in regex.captures_iter(&search_string) {
            let g0 = m.get(0).unwrap();

            match replace_groups {
                Some(groups) => {
                    for (idx, g) in m.iter().enumerate() {
                        if idx == 0 {
                            continue;
                        }

                        if let Some(g) = g {
                            if groups.contains(&(idx as u8)) {
                                process_text(
                                    &search_string[pos..g.start()],
                                    &mut rv,
                                    &mut replacement_chunks,
                                );
                                self.insert_replacement_chunks(g.as_str(), &mut rv);
                                pos = g.end();
                            }
                        }
                    }
                }
                None => {
                    process_text(
                        &search_string[pos..g0.start()],
                        &mut rv,
                        &mut replacement_chunks,
                    );
                    self.insert_replacement_chunks(g0.as_str(), &mut rv);
                    pos = g0.end();
                }
            }

            process_text(
                &search_string[pos..g0.end()],
                &mut rv,
                &mut replacement_chunks,
            );
            pos = g0.end();
        }

        process_text(&search_string[pos..], &mut rv, &mut replacement_chunks);

        (rv, meta)
    }

    /// Applies the rule to the given value.
    ///
    /// In case `Err` is returned the caller is expected to try the next rule.  If
    /// `Ok` is returned then no further modifications are applied.
    fn process_value(
        &self,
        value: Annotated<Value>,
        kind: PiiKind,
    ) -> Result<Annotated<Value>, Annotated<Value>> {
        let _kind = kind;
        match self.spec.ty {
            // pattern matches are not implemented for non strings
            RuleType::Pattern { .. }
            | RuleType::Email
            | RuleType::Ipv4
            | RuleType::Ipv6
            | RuleType::Ip
            | RuleType::Creditcard => Err(value),
            RuleType::Remove => {
                return Ok(self.replace_value(value));
            }
            RuleType::RemovePair { ref key_pattern } => {
                if let Some(ref path) = value.meta().path() {
                    if key_pattern.0.is_match(&path.to_string()) {
                        return Ok(self.replace_value(value));
                    }
                }
                Err(value)
            }
        }
    }
}

impl<'a> RuleBasedPiiProcessor<'a> {
    /// Creates a new rule based PII processor from a config.
    pub fn new(cfg: &'a RuleConfig) -> Result<RuleBasedPiiProcessor<'a>, BadRuleConfig> {
        let mut applications = BTreeMap::new();

        for (&pii_kind, cfg_applications) in &cfg.applications {
            let mut rules = vec![];
            for application in cfg_applications {
                if let Some(rule_spec) = cfg.rules.get(application) {
                    rules.push(Rule {
                        id: application.as_str(),
                        spec: rule_spec,
                    });
                } else {
                    return Err(BadRuleConfig::BadReference(application.to_string()));
                }
            }
            applications.insert(pii_kind, rules);
        }

        Ok(RuleBasedPiiProcessor {
            cfg: cfg,
            applications: applications,
        })
    }

    /// Returns a reference to the config that created the processor.
    pub fn config(&self) -> &RuleConfig {
        self.cfg
    }

    /// Processes a root value (annotated event for instance)
    pub fn process_root_value<T: ProcessAnnotatedValue>(
        &self,
        value: Annotated<T>,
    ) -> Annotated<T> {
        ProcessAnnotatedValue::process_annotated_value(
            Annotated::from(value),
            self,
            &ValueInfo::default(),
        )
    }
}

impl<'a> PiiProcessor for RuleBasedPiiProcessor<'a> {
    fn pii_process_chunks(
        &self,
        chunks: Vec<Chunk>,
        meta: Meta,
        pii_kind: PiiKind,
    ) -> Result<(Vec<Chunk>, Meta), (Vec<Chunk>, Meta)> {
        let mut replaced = false;
        let mut rv = (chunks, meta);

        if let Some(rules) = self.applications.get(&pii_kind) {
            for rule in rules {
                rv = match rule.process_chunks(rv.0, rv.1) {
                    Ok(val) => {
                        replaced = true;
                        val
                    }
                    Err(val) => val,
                };
            }
        }

        if replaced {
            Ok(rv)
        } else {
            Err(rv)
        }
    }

    fn pii_process_value(&self, mut value: Annotated<Value>, kind: PiiKind) -> Annotated<Value> {
        if let Some(rules) = self.applications.get(&kind) {
            for rule in rules {
                value = match rule.process_value(value, kind) {
                    Ok(value) => return value,
                    Err(value) => value,
                };
            }
        }
        value
    }
}

#[test]
fn test_basic_stripping() {
    use common::Map;
    use meta::Remark;
    use serde_json;

    let cfg: RuleConfig = serde_json::from_str(
        r#"{
        "rules": {
            "path_username": {
                "type": "pattern",
                "pattern": "(?i)(?:\b[a-zA-Z]:)?(?:[/\\\\](?:users|home)[/\\\\])([^/\\\\\\s]+)",
                "replaceGroups": [1],
                "redaction": {
                    "method": "replace",
                    "newValue": "[username]"
                },
                "note": "username in path"
            },
            "creditcard_number": {
                "type": "pattern",
                "pattern": "\\d{4}[- ]?\\d{4,6}[- ]?\\d{4,5}(?:[- ]?\\d{4})",
                "redaction": {
                    "method": "mask",
                    "maskChar": "*",
                    "charsToIgnore": "- ",
                    "range": [0, -4]
                },
                "note": "creditcard number"
            },
            "email_address": {
                "type": "pattern",
                "pattern": "[a-z0-9!#$%&'*+/=?^_`{|}~.-]+@[a-z0-9-]+(\\.[a-z0-9-]+)*",
                "redaction": {
                    "method": "mask",
                    "maskChar": "*",
                    "charsToIgnore": "@."
                },
                "note": "potential email address"
            },
            "remove_foo": {
                "type": "removePair",
                "keyPattern": "foo",
                "redaction": {
                    "method": "replace",
                    "newValue": "whatever"
                }
            },
            "remove_ip": {
                "type": "remove",
                "note": "IP address removed"
            },
            "hash_ip": {
                "type": "pattern",
                "pattern": "\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}",
                "redaction": {
                    "method": "hash",
                    "algorithm": "HMAC-SHA256",
                    "key": "DEADBEEF1234"
                },
                "note": "IP address hashed"
            }
        },
        "applications": {
            "freeform": ["path_username", "creditcard_number", "email_address", "hash_ip"],
            "ip": ["remove_ip"],
            "databag": ["remove_foo"]
        }
    }"#,
    ).unwrap();

    #[derive(ProcessAnnotatedValue, Debug, Deserialize, Serialize, Clone)]
    struct Event {
        #[process_annotated_value(pii_kind = "freeform")]
        message: Annotated<String>,
        #[process_annotated_value(pii_kind = "databag")]
        extra: Annotated<Map<Value>>,
        #[process_annotated_value(pii_kind = "ip")]
        ip: Annotated<String>,
    }

    let event = Annotated::<Event>::from_str(r#"
        {
            "message": "Hello peter@gmail.com.  You signed up with card 1234-1234-1234-1234. Your home folder is C:\\Users\\peter. Look at our compliance from 127.0.0.1",
            "extra": {
                "foo": 42,
                "bar": true
            },
            "ip": "192.168.1.1"
        }
    "#).unwrap();

    let processor = RuleBasedPiiProcessor::new(&cfg).unwrap();
    let processed_event = processor.process_root_value(event);
    let new_event = processed_event.clone().0.unwrap();

    let message = new_event.message.value().unwrap();
    println!("{:#?}", &new_event);
    assert_eq!(
        message,
        "Hello *****@*****.***.  You signed up with card ****-****-****-1234. \
         Your home folder is C:\\Users\\[username] Look at our compliance \
         from 5A2DF387CD660E9F3E0AB20F9E7805450D56C5DACE9B959FC620C336E2B5D09A"
    );
    assert_eq!(
        new_event.message.meta(),
        &Meta {
            remarks: vec![
                Remark::with_range(
                    Note::new("email_address", Some("potential email address")),
                    (6, 21),
                ),
                Remark::with_range(
                    Note::new("creditcard_number", Some("creditcard number")),
                    (48, 67),
                ),
                Remark::with_range(
                    Note::new("path_username", Some("username in path")),
                    (98, 108),
                ),
                Remark::with_range(Note::new("hash_ip", Some("IP address hashed")), (137, 201)),
            ],
            errors: vec![],
            original_length: Some(142),
            path: None,
        }
    );

    let foo = new_event.extra.value().unwrap().get("foo").unwrap();
    assert!(foo.value().is_none());
    assert_eq!(
        foo.meta(),
        &Meta {
            remarks: vec![Remark::new(Note::well_known("remove_foo"))],
            errors: vec![],
            original_length: None,
            path: None,
        }
    );

    let ip = &new_event.ip;
    assert!(ip.value().is_none());
    assert_eq!(
        ip.meta(),
        &Meta {
            remarks: vec![Remark::new(Note::new(
                "remove_ip",
                Some("IP address removed"),
            ))],
            errors: vec![],
            original_length: None,
            path: None,
        }
    );

    let value = processed_event.to_string().unwrap();
    assert_eq!(value, "{\"message\":\"Hello *****@*****.***.  You signed up with card ****-****-****-1234. Your home folder is C:\\\\Users\\\\[username] Look at our compliance from 5A2DF387CD660E9F3E0AB20F9E7805450D56C5DACE9B959FC620C336E2B5D09A\",\"extra\":{\"bar\":true,\"foo\":null},\"ip\":null,\"metadata\":{\"extra\":{\"foo\":{\"\":{\"remarks\":[[[\"remove_foo\"]]]}}},\"ip\":{\"\":{\"remarks\":[[[\"remove_ip\",\"IP address removed\"]]]}},\"message\":{\"\":{\"original_length\":142,\"remarks\":[[[\"email_address\",\"potential email address\"],[6,21]],[[\"creditcard_number\",\"creditcard number\"],[48,67]],[[\"path_username\",\"username in path\"],[98,108]],[[\"hash_ip\",\"IP address hashed\"],[137,201]]]}}}}");
}
