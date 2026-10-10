//! OpenAPI 3.x spec import.
//!
//! Turns "I have a spec, I need a mock server" into a single command:
//!
//! ```text
//! mimic import-openapi ./spec.yaml --out ./mocks/generated
//! ```
//!
//! The importer reads an OpenAPI 3.x document (JSON or YAML) and writes one
//! ordinary Mimic mock file per operation + response status. The output is a
//! plain `MockConfig`, JSON by default or YAML with `--format yaml` — nothing
//! about it is special either way, so it can be hand-edited afterwards and
//! Mimic's file-based, zero-config model stays intact.
//!
//! Only the subset of OpenAPI that affects a mock response is parsed
//! (`paths`, operations, `responses`, `content`, examples, and schemas). The
//! document is walked as a `serde_json::Value` rather than deserialized into a
//! full typed model, which keeps the binary small and makes unknown or
//! vendor-specific fields a non-event instead of a parse failure.

use crate::types::MockConfig;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Default output directory when `--out` is not given.
pub const DEFAULT_OUT_DIR: &str = "./mocks/generated";

/// Default status treated as an operation's "primary" response — its file
/// gets no status suffix.
pub const DEFAULT_STATUS: u16 = 200;

/// Operation keys recognized inside a Path Item Object, in the order the
/// importer emits them. Anything else (`parameters`, `summary`, `x-*`) is
/// ignored.
const HTTP_METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Guards against `$ref` cycles that a `path`-based check can't catch (a
/// schema nesting itself through inline `properties`, say) and against specs
/// deep enough to blow the stack.
const MAX_STUB_DEPTH: usize = 32;

/// A non-primary status's file is suffixed with `.disabled` after its normal
/// extension (`get_a_404.json.disabled`, `get_a_404.yaml.disabled`).
///
/// A spec's alternative responses (`404`, `500`, …) share a path and method
/// with the primary one and carry no matcher to tell them apart, so leaving
/// them all live would make the served response depend on directory read
/// order. The loader only reads `.json`/`.yaml`/`.yml`, so this suffix parks
/// them next to the mock they belong to, inert until renamed — which is what
/// "enable selectively" has to mean in a directory of files.
const DISABLED_SUFFIX: &str = ".disabled";

pub const USAGE: &str = "\
Usage: mimic import-openapi <spec.yaml|spec.json> [options]

Generate Mimic mock files from an OpenAPI 3.x spec.

One live mock file is written per operation, from its primary response.
Other documented statuses are written alongside as
<name>_<status>.<ext>.disabled — rename to drop the suffix to enable one.

Options:
  --out <dir>          Output directory (default: ./mocks/generated)
  --status <code>      Status treated as each operation's primary response;
                       its file is the live one (default: 200)
  --format <fmt>       Output format for generated mock files: json or yaml
                       (default: json)
  --base-path <path>   Prefix every generated mock's path instead of the one
                       the spec's servers[0].url carries; an empty value ('')
                       drops the prefix entirely (default: apply servers[0])
  --force              Write into a non-empty output directory, overwriting
                       files of the same name
  --brace-params       Emit path parameters as {id} instead of :id
  -h, --help           Show this help
";

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug)]
pub enum ImportError {
    /// Bad command line — the caller should show `USAGE`.
    Usage(String),
    Io(String),
    /// The file isn't valid JSON or YAML.
    Parse(String),
    /// The file parsed, but isn't an OpenAPI 3.x spec we can import.
    Invalid(String),
    /// The output directory already holds files and `--force` wasn't given.
    OutputNotEmpty(PathBuf),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::Usage(msg) => write!(f, "{}", msg),
            ImportError::Io(msg) => write!(f, "{}", msg),
            ImportError::Parse(msg) => write!(f, "{}", msg),
            ImportError::Invalid(msg) => write!(f, "{}", msg),
            ImportError::OutputNotEmpty(dir) => write!(
                f,
                "output directory '{}' is not empty; pass --force to write into it \
                 (existing files with the same name will be overwritten)",
                dir.display()
            ),
        }
    }
}

/// Non-fatal diagnostics go to stderr rather than through `tracing`: the
/// importer runs before logging is initialized and its output should be
/// visible without setting `RUST_LOG`.
fn warn(message: impl AsRef<str>) {
    eprintln!("warning: {}", message.as_ref());
}

// ============================================================================
// Options
// ============================================================================

/// How path parameters are rendered in the generated `path` field. Mimic's
/// matcher accepts both, so this is purely a style choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStyle {
    /// `/users/:id` — matches the style used throughout Mimic's docs.
    Colon,
    /// `/users/{id}` — passes the OpenAPI template through untouched.
    Brace,
}

/// Serialization format for generated mock files, chosen with `--format`.
/// Same `MockConfig` schema either way — this only changes how it's written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Json,
    Yaml,
}

impl OutputFormat {
    /// The bare extension (no dot) a generated file gets, and the name
    /// `--format` accepts for it.
    fn extension(self) -> &'static str {
        match self {
            OutputFormat::Json => "json",
            OutputFormat::Yaml => "yaml",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub spec_path: PathBuf,
    pub out_dir: PathBuf,
    pub default_status: u16,
    pub force: bool,
    pub path_style: PathStyle,
    pub format: OutputFormat,
    /// `None`: derive the base path from the spec's `servers`. `Some("")`:
    /// explicitly drop it. `Some(path)`: override.
    pub base_path: Option<String>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            spec_path: PathBuf::new(),
            out_dir: PathBuf::from(DEFAULT_OUT_DIR),
            default_status: DEFAULT_STATUS,
            force: false,
            path_style: PathStyle::Colon,
            format: OutputFormat::default(),
            base_path: None,
        }
    }
}

/// Parse the arguments that follow `import-openapi`.
pub fn parse_args(args: &[String]) -> Result<ImportOptions, ImportError> {
    let mut options = ImportOptions::default();
    let mut spec: Option<PathBuf> = None;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "-h" | "--help" => return Err(ImportError::Usage(String::new())),
            "--force" => options.force = true,
            "--brace-params" => options.path_style = PathStyle::Brace,
            "--out" | "--status" | "--format" | "--base-path" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| ImportError::Usage(format!("{} requires a value", arg)))?;
                match arg {
                    "--out" => options.out_dir = PathBuf::from(value),
                    "--base-path" => options.base_path = Some(normalize_base_path(value)),
                    "--status" => {
                        options.default_status = value.parse::<u16>().map_err(|_| {
                            ImportError::Usage(format!(
                                "--status expects an HTTP status code, got '{}'",
                                value
                            ))
                        })?;
                    }
                    _ => {
                        options.format = match value.to_ascii_lowercase().as_str() {
                            "json" => OutputFormat::Json,
                            "yaml" | "yml" => OutputFormat::Yaml,
                            other => {
                                return Err(ImportError::Usage(format!(
                                    "--format expects 'json' or 'yaml', got '{}'",
                                    other
                                )))
                            }
                        };
                    }
                }
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(ImportError::Usage(format!("unknown option '{}'", other)));
            }
            other => {
                if spec.is_some() {
                    return Err(ImportError::Usage(format!(
                        "unexpected extra argument '{}'; only one spec file may be given",
                        other
                    )));
                }
                spec = Some(PathBuf::from(other));
            }
        }
        index += 1;
    }

    options.spec_path =
        spec.ok_or_else(|| ImportError::Usage("missing spec file argument".to_string()))?;
    Ok(options)
}

// ============================================================================
// Spec
// ============================================================================

/// A parsed OpenAPI 3.x document.
///
/// Holds the raw document so `$ref` pointers can be resolved against the
/// root, which is the only thing resolution needs.
#[derive(Debug)]
pub struct OpenApiSpec {
    root: Value,
}

impl OpenApiSpec {
    /// Parse a spec from JSON or YAML source text.
    pub fn parse(source: &str) -> Result<Self, ImportError> {
        let root = if source.trim_start().starts_with('{') {
            serde_json::from_str(source)
                .map_err(|e| ImportError::Parse(format!("spec is not valid JSON: {}", e)))?
        } else {
            let yaml: serde_yaml::Value = serde_yaml::from_str(source)
                .map_err(|e| ImportError::Parse(format!("spec is not valid YAML: {}", e)))?;
            yaml_to_json(yaml)?
        };
        Self::from_value(root)
    }

    /// Read and parse a spec from disk.
    pub fn from_path(path: &Path) -> Result<Self, ImportError> {
        let source = fs::read_to_string(path)
            .map_err(|e| ImportError::Io(format!("cannot read '{}': {}", path.display(), e)))?;
        Self::parse(&source)
    }

    fn from_value(root: Value) -> Result<Self, ImportError> {
        let Some(object) = root.as_object() else {
            return Err(ImportError::Invalid(
                "spec root must be a JSON object / YAML mapping".to_string(),
            ));
        };

        if let Some(version) = object.get("swagger").and_then(Value::as_str) {
            return Err(ImportError::Invalid(format!(
                "Swagger {} is not supported; convert the spec to OpenAPI 3.x first",
                version
            )));
        }

        match object.get("openapi").and_then(Value::as_str) {
            Some(version) if version.starts_with("3.") => {}
            Some(version) => {
                return Err(ImportError::Invalid(format!(
                    "unsupported OpenAPI version '{}'; only 3.x is supported",
                    version
                )))
            }
            None => return Err(ImportError::Invalid(
                "spec has no top-level 'openapi' version string; is this an OpenAPI 3.x document?"
                    .to_string(),
            )),
        }

        if !object.get("paths").is_some_and(Value::is_object) {
            return Err(ImportError::Invalid(
                "spec has no 'paths' object; nothing to generate".to_string(),
            ));
        }

        Ok(Self { root })
    }

    fn paths(&self) -> &Map<String, Value> {
        // `from_value` rejected any spec without a `paths` object.
        self.root["paths"].as_object().expect("validated at parse")
    }

    /// Follow a chain of `$ref` links until a concrete node is reached.
    ///
    /// Returns `None` for external refs, unresolvable pointers, and cycles —
    /// every caller degrades to an empty stub rather than failing the import.
    fn resolve<'a>(&'a self, value: &'a Value) -> Option<&'a Value> {
        let mut current = value;
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(reference) = current.get("$ref").and_then(Value::as_str) {
            if !seen.insert(reference) {
                warn(format!("circular $ref chain at '{}'", reference));
                return None;
            }
            current = self.resolve_pointer(reference)?;
        }
        Some(current)
    }

    /// Resolve a single local `#/...` JSON pointer against the spec root.
    fn resolve_pointer(&self, reference: &str) -> Option<&Value> {
        let Some(pointer) = reference.strip_prefix('#') else {
            warn(format!(
                "external $ref '{}' is not supported; using an empty stub",
                reference
            ));
            return None;
        };
        if pointer.is_empty() {
            return Some(&self.root);
        }
        // OpenAPI tooling sometimes percent-encodes pointer segments
        // (`%7Bid%7D`); serde_json's pointer handles ~0/~1 escaping itself.
        let decoded = urlencoding::decode(pointer)
            .map(|value| value.into_owned())
            .unwrap_or_else(|_| pointer.to_string());
        let target = self.root.pointer(&decoded);
        if target.is_none() {
            warn(format!(
                "$ref '{}' does not resolve; using an empty stub",
                reference
            ));
        }
        target
    }

    /// The base path a Server Object list contributes to every generated
    /// mock's `path` field.
    ///
    /// A server's scheme, host, and port are irrelevant to a mock and
    /// ignored; its path component is what a client built from the spec
    /// prepends to every path, so that is what the mock must serve
    /// (#118). The first entry wins — a spec usually lists several
    /// deployment targets of the same API — with a warning naming the
    /// others and pointing at `--base-path`. Server *variables* are
    /// resolved from their `default` values; one inside the path without
    /// a default is dropped with a warning rather than emitted literally.
    fn base_path_from_servers(&self, servers: &Value, label: &str) -> Option<String> {
        let list = self.resolve(servers).and_then(Value::as_array)?;
        if list.is_empty() {
            return None;
        }
        if list.len() > 1 {
            warn(format!(
                "{label} lists {} servers; using servers[0]; pass --base-path to use another",
                list.len()
            ));
        }
        let Some(server) = self.resolve(&list[0]).and_then(Value::as_object) else {
            warn(format!(
                "ignoring the base path of {label}: servers[0] is not a Server Object"
            ));
            return None;
        };
        let Some(url) = server.get("url").and_then(Value::as_str) else {
            warn(format!(
                "ignoring the base path of {label}: servers[0] has no url"
            ));
            return None;
        };

        let (url, missing) =
            resolve_server_variables(url, server.get("variables").and_then(Value::as_object));
        let mut base = server_path_component(&url)?;
        for name in missing {
            if base.contains(&format!("{{{name}}}")) {
                warn(format!(
                    "{label}: server variable '{name}' has no default; dropping it from the base path"
                ));
                base = collapse_slashes(
                    base.replace(&format!("{{{name}}}"), "")
                        .trim_end_matches('/'),
                );
            }
        }
        if base.is_empty() {
            None
        } else {
            Some(base.to_string())
        }
    }

    /// The document-level base path. Nothing here when the spec has no
    /// `servers` — today's behavior for the vast majority of specs.
    fn top_level_base_path(&self) -> Option<String> {
        match self.root.get("servers") {
            Some(servers) => self.base_path_from_servers(servers, "the spec's servers"),
            None => None,
        }
    }

    /// Body for one Response Object: an explicit example if the spec has one,
    /// otherwise a stub built from the response schema.
    fn response_body(&self, response: &Value) -> Value {
        let Some(response) = self.resolve(response) else {
            return empty_object();
        };
        let Some(content) = response.get("content").and_then(Value::as_object) else {
            // 204s and other body-less responses.
            return empty_object();
        };
        let Some(media) = pick_media_type(content) else {
            return empty_object();
        };

        if let Some(example) = media.get("example") {
            return example.clone();
        }

        // `examples` is a map of named Example Objects; take the first and
        // read its `value`. Named examples may themselves be `$ref`s.
        if let Some(named) = media.get("examples").and_then(Value::as_object) {
            for example in named.values() {
                if let Some(resolved) = self.resolve(example) {
                    if let Some(value) = resolved.get("value") {
                        return value.clone();
                    }
                }
            }
        }

        match media.get("schema") {
            Some(schema) => self.stub_from_schema(schema, &mut Vec::new(), 0),
            None => empty_object(),
        }
    }

    /// Build a placeholder value from a JSON Schema, recursing through
    /// `properties` and `items`.
    ///
    /// `active` is the stack of `$ref` pointers currently being expanded, so a
    /// self-referential schema (`Node.children: [Node]`) collapses to `{}` at
    /// the point of recursion instead of looping forever — while a schema
    /// referenced twice in sibling positions still expands both times.
    fn stub_from_schema(&self, schema: &Value, active: &mut Vec<String>, depth: usize) -> Value {
        if depth >= MAX_STUB_DEPTH {
            return empty_object();
        }
        let Some(object) = schema.as_object() else {
            return empty_object();
        };

        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            if active.iter().any(|active| active == reference) {
                return empty_object();
            }
            let Some(target) = self.resolve_pointer(reference) else {
                return empty_object();
            };
            active.push(reference.to_string());
            let stub = self.stub_from_schema(target, active, depth + 1);
            active.pop();
            return stub;
        }

        // Anything the spec states outright beats a generated placeholder.
        if let Some(example) = object.get("example") {
            return example.clone();
        }
        if let Some(default) = object.get("default") {
            return default.clone();
        }
        if let Some(first) = object
            .get("enum")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
        {
            return first.clone();
        }

        if let Some(variants) = object.get("allOf").and_then(Value::as_array) {
            let mut merged = Map::new();
            for variant in variants {
                if let Value::Object(fields) = self.stub_from_schema(variant, active, depth + 1) {
                    merged.extend(fields);
                }
            }
            return Value::Object(merged);
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(first) = object
                .get(key)
                .and_then(Value::as_array)
                .and_then(|variants| variants.first())
            {
                return self.stub_from_schema(first, active, depth + 1);
            }
        }

        match schema_type(object) {
            Some("object") | None if object.contains_key("properties") => {
                let mut stub = Map::new();
                if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                    for (name, property) in properties {
                        stub.insert(
                            name.clone(),
                            self.stub_from_schema(property, active, depth + 1),
                        );
                    }
                }
                Value::Object(stub)
            }
            Some("object") => empty_object(),
            Some("array") => match object.get("items") {
                // One sample element makes a list endpoint useful straight
                // out of the importer; an untyped array has nothing to build
                // an element from.
                Some(items) if !items.is_null() => {
                    Value::Array(vec![self.stub_from_schema(items, active, depth + 1)])
                }
                _ => Value::Array(Vec::new()),
            },
            Some("string") => {
                Value::String(stub_string(object.get("format").and_then(Value::as_str)))
            }
            Some("integer") => Value::from(0),
            Some("number") => Value::from(0.0),
            Some("boolean") => Value::Bool(false),
            Some("null") => Value::Null,
            // Untyped, or a type we don't model: an empty object is the
            // safest thing to hand a user who will edit it anyway.
            _ => empty_object(),
        }
    }
}

// ============================================================================
// Generation
// ============================================================================

/// Generate `(filename, MockConfig)` pairs — one per operation and documented
/// response status — in a deterministic order.
///
/// The response matching `default_status` is treated as the operation's
/// primary one and is the only live mock for that operation
/// (`get_users_id.json`). Every other status is written alongside it,
/// status-suffixed and inert (`get_users_id_404.json.disabled`), ready to be
/// enabled by dropping the suffix. They are alternative responses, not a
/// call-order sequence, so they are deliberately *not* folded into a single
/// `sequence` mock either.
pub fn generate_mocks(
    spec: &OpenApiSpec,
    default_status: u16,
    path_style: PathStyle,
    format: OutputFormat,
    base_path: &str,
) -> Vec<(String, MockConfig)> {
    let mut generated: Vec<(String, MockConfig)> = Vec::new();
    let mut used_names: HashSet<String> = HashSet::new();

    for (template, path_item) in spec.paths() {
        // `paths` may also carry `x-` extension keys; real paths start with /.
        if !template.starts_with('/') {
            continue;
        }
        let Some(path_item) = spec.resolve(path_item).and_then(Value::as_object) else {
            warn(format!("skipping path '{}': unusable path item", template));
            continue;
        };

        // A path item may carry its own `servers`. OpenAPI says a non-empty
        // path-level list replaces the document-level one, so it wins for
        // every operation under it — including a path-level server with no
        // path component, which means no prefix for this path where the
        // global has one.
        let path_servers = path_item
            .get("servers")
            .filter(|servers| servers.as_array().is_some_and(|list| !list.is_empty()));
        let translated = translate_path(template, path_style);

        for method in HTTP_METHODS {
            let Some(operation) = path_item.get(method) else {
                continue;
            };
            let Some(operation) = spec.resolve(operation).and_then(Value::as_object) else {
                warn(format!(
                    "skipping {} {}: unusable operation object",
                    method.to_uppercase(),
                    template
                ));
                continue;
            };

            // `servers` may also live on the operation — the most specific
            // non-empty list wins, operation over path item over document.
            let (base, label) = match operation.get("servers") {
                Some(servers) if servers.as_array().is_some_and(|list| !list.is_empty()) => (
                    Some(servers),
                    format!("the servers of {} {}", method.to_uppercase(), template),
                ),
                _ => (path_servers, format!("the servers of path `{}`", template)),
            };
            let base = match base {
                Some(servers) => spec
                    .base_path_from_servers(servers, &label)
                    .unwrap_or_default(),
                None => base_path.to_string(),
            };
            let mock_path = prepend_base(&base, &translated);

            let responses: Vec<GeneratedResponse> = match operation
                .get("responses")
                .and_then(Value::as_object)
                .filter(|responses| !responses.is_empty())
            {
                Some(responses) => responses
                    .iter()
                    .filter_map(|(key, response)| {
                        let Some(status) = status_code(key, default_status) else {
                            warn(format!(
                                "{} {}: ignoring unrecognized response key '{}'",
                                method.to_uppercase(),
                                template,
                                key
                            ));
                            return None;
                        };
                        Some(GeneratedResponse {
                            status,
                            is_catch_all: key.eq_ignore_ascii_case("default"),
                            body: spec.response_body(response),
                        })
                    })
                    .collect(),
                // An operation with no documented responses still deserves a
                // route, so the spec's shape is reachable immediately.
                None => vec![GeneratedResponse {
                    status: default_status,
                    is_catch_all: false,
                    body: empty_object(),
                }],
            };

            let primary = choose_primary(&responses, default_status);

            for (index, GeneratedResponse { status, body, .. }) in responses.into_iter().enumerate()
            {
                let name = unique_name(
                    &base_filename(method, template, status, index == primary, format),
                    &mut used_names,
                );
                let response = body;
                generated.push((
                    name,
                    MockConfig {
                        method: method.to_uppercase(),
                        path: mock_path.clone(),
                        status,
                        response,
                        consume_body: false,
                        query_params: None,
                        headers: None,
                        body: None,
                        delay_ms: None,
                        response_headers: None,
                        source: None,
                        sequence: None,
                        tags: Vec::new(),
                        response_file: None,
                        template: None,
                        response_bytes: None,
                    },
                ));
            }
        }
    }

    generated
}

/// Write generated mocks to `out_dir`.
///
/// Refuses a non-empty directory without `force`, so a second import can't
/// silently clobber mocks that were hand-edited after the first one.
pub fn write_mocks(
    mocks: &[(String, MockConfig)],
    out_dir: &Path,
    force: bool,
    format: OutputFormat,
) -> Result<Vec<PathBuf>, ImportError> {
    if !force && dir_has_entries(out_dir)? {
        return Err(ImportError::OutputNotEmpty(out_dir.to_path_buf()));
    }

    fs::create_dir_all(out_dir).map_err(|e| {
        ImportError::Io(format!(
            "cannot create output directory '{}': {}",
            out_dir.display(),
            e
        ))
    })?;

    let mut written = Vec::with_capacity(mocks.len());
    for (name, config) in mocks {
        let path = out_dir.join(name);
        if path.exists() {
            warn(format!("overwriting existing file '{}'", path.display()));
        }
        let body = match format {
            OutputFormat::Json => {
                let mut body = serde_json::to_string_pretty(config).map_err(|e| {
                    ImportError::Io(format!("cannot serialize mock '{}': {}", name, e))
                })?;
                body.push('\n');
                body
            }
            OutputFormat::Yaml => serde_yaml::to_string(config)
                .map_err(|e| ImportError::Io(format!("cannot serialize mock '{}': {}", name, e)))?,
        };
        fs::write(&path, body)
            .map_err(|e| ImportError::Io(format!("cannot write '{}': {}", path.display(), e)))?;
        written.push(path);
    }

    Ok(written)
}

/// Run the whole `import-openapi` subcommand: parse args, parse the spec,
/// resolve the base path, generate, write.
///
/// Returns the files written along with the base path actually applied
/// (empty when none), so the summary line can say where the mocks live
/// rather than leaving the prefix to be discovered at request time (:118).
pub fn run_import(args: &[String]) -> Result<(Vec<PathBuf>, String), ImportError> {
    let options = parse_args(args)?;
    let spec = OpenApiSpec::from_path(&options.spec_path)?;
    let from_servers = spec.top_level_base_path();
    let base_path = match options.base_path.as_deref() {
        // `--base-path ''`: drop the prefix, whatever the spec offered.
        Some("") => {
            if from_servers.is_some() {
                println!("Dropping the spec's base path (--base-path '')");
            }
            String::new()
        }
        Some(flag) => {
            if from_servers
                .as_deref()
                .is_some_and(|derived| derived != flag)
            {
                println!("Applying base path {flag} (--base-path overrides the spec's servers)");
            }
            flag.to_string()
        }
        None => match from_servers {
            Some(derived) => {
                println!("Applying base path {derived} from servers[0]");
                derived
            }
            None => String::new(),
        },
    };
    let mocks = generate_mocks(
        &spec,
        options.default_status,
        options.path_style,
        options.format,
        &base_path,
    );

    if mocks.is_empty() {
        warn("spec contained no importable operations; nothing was written");
        return Ok((Vec::new(), base_path));
    }

    let written = write_mocks(&mocks, &options.out_dir, options.force, options.format)?;
    Ok((written, base_path))
}

// ============================================================================
// Helpers
// ============================================================================

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// `type` is a string in OpenAPI 3.0 and may be a list in 3.1
/// (`["string", "null"]`); take the first non-null entry either way.
fn schema_type(schema: &Map<String, Value>) -> Option<&str> {
    match schema.get("type") {
        Some(Value::String(name)) => Some(name.as_str()),
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .find(|name| *name != "null"),
        _ => None,
    }
}

fn stub_string(format: Option<&str>) -> String {
    match format {
        Some("date-time") => "2024-01-01T00:00:00Z",
        Some("date") => "2024-01-01",
        Some("uuid") => "00000000-0000-0000-0000-000000000000",
        Some("email") => "user@example.com",
        Some("uri") | Some("url") => "https://example.com",
        _ => "string",
    }
    .to_string()
}

/// Pick the media type to mock from a Content Object: JSON if it's on offer,
/// otherwise whatever comes first.
fn pick_media_type(content: &Map<String, Value>) -> Option<&Value> {
    if let Some(json) = content.get("application/json") {
        return Some(json);
    }
    if let Some((_, value)) = content.iter().find(|(name, _)| name.contains("json")) {
        return Some(value);
    }
    content.values().next()
}

/// Map a Responses Object key to a concrete status: `"200"`, the `"default"`
/// catch-all, and wildcard ranges like `"4XX"` are all accepted.
fn status_code(key: &str, default_status: u16) -> Option<u16> {
    if key.eq_ignore_ascii_case("default") {
        return Some(default_status);
    }
    if let Ok(status) = key.parse::<u16>() {
        return (100..=599).contains(&status).then_some(status);
    }
    let mut chars = key.chars();
    let leading = chars.next()?.to_digit(10)?;
    if (1..=5).contains(&leading) && chars.all(|c| c == 'X' || c == 'x') && key.len() == 3 {
        return Some(leading as u16 * 100);
    }
    None
}

/// Render an OpenAPI path template in the requested style. Mimic's matcher
/// understands `{id}` natively, so `Brace` is a pass-through.
fn translate_path(template: &str, style: PathStyle) -> String {
    if style == PathStyle::Brace {
        return template.to_string();
    }
    template
        .split('/')
        .map(|segment| {
            // Mirror the matcher's own brace test, so a segment it would treat
            // as a literal (`{}`) stays literal here too.
            if segment.len() > 2 && segment.starts_with('{') && segment.ends_with('}') {
                format!(":{}", &segment[1..segment.len() - 1])
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Normalize a `--base-path` value: `/api`, `api`, `/api/`, and `//api` all
/// mean `/api`; emptiness and `/` both mean "no base path", which is how
/// `--base-path ''` says "drop the spec's prefix".
fn normalize_base_path(raw: &str) -> String {
    let core = raw.trim_matches('/');
    if core.is_empty() {
        String::new()
    } else {
        format!("/{core}")
    }
}

/// Collapse runs of slashes left behind by a dropped server variable, and
/// trim the trailing one. Never eats the leading `/`.
fn collapse_slashes(path: &str) -> String {
    let mut collapsed = path.to_string();
    while collapsed.contains("//") {
        collapsed = collapsed.replace("//", "/");
    }
    collapsed
}

/// The path component of a Server Object's URL — the only part a mock cares
/// about. Scheme, host, and port are ignored; a relative URL (legal in
/// OpenAPI, resolved against the document) is its own path. Returns `None`
/// for a server with no path, which is the no-op case and by far the most
/// common.
fn server_path_component(url: &str) -> Option<String> {
    let rest = match url.find("://") {
        Some(scheme_end) => {
            let authority_and_path = &url[scheme_end + 3..];
            let slash = authority_and_path.find('/')?;
            &authority_and_path[slash..]
        }
        // Relative server URL: the whole string is the path.
        None => url,
    };
    let path = collapse_slashes(rest.trim_end_matches('/'));
    match path.as_str() {
        "" | "/" => None,
        other if other.starts_with('/') => Some(other.to_string()),
        other => Some(format!("/{other}")),
    }
}

/// Substitute `{name}` occurrences in a server URL with each variable's
/// `default` from the Server Object's `variables`. Returns the resolved URL
/// and the names that had no default — including names with no `variables`
/// block at all — which the caller warns about and drops from the base path
/// rather than emitting literally (#118).
fn resolve_server_variables(
    url: &str,
    variables: Option<&Map<String, Value>>,
) -> (String, Vec<String>) {
    let mut resolved = url.to_string();
    let mut missing: Vec<String> = Vec::new();
    let mut start = 0;
    while let Some(open_rel) = resolved[start..].find('{') {
        let open = start + open_rel;
        let Some(close_rel) = resolved[open..].find('}') else {
            break;
        };
        let close = open + close_rel;
        let name = &resolved[open + 1..close];
        match variables
            .and_then(|vars| vars.get(name))
            .and_then(|v| v.get("default"))
            .and_then(Value::as_str)
        {
            Some(value) => {
                resolved.replace_range(open..close + 1, value);
                start = open + value.len();
            }
            None => {
                if !missing.iter().any(|n| n == name) {
                    missing.push(name.to_string());
                }
                start = close + 1;
            }
        }
    }
    (resolved, missing)
}

/// Attach a base path to a translated (or raw) path template. Kept boring:
/// no doubled slashes, and the document root `/` under a base stays just the
/// base — `GET /` on a `/v1` deployment is `GET /v1`.
fn prepend_base(base: &str, path: &str) -> String {
    if base.is_empty() {
        return path.to_string();
    }
    if path == "/" {
        return base.to_string();
    }
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// `GET /users/{id}` primary -> `get_users_id.json`;
/// `GET /users/{id}` + 404 -> `get_users_id_404.json.disabled` (or the `.yaml`
/// equivalents, with `--format yaml`).
fn base_filename(
    method: &str,
    template: &str,
    status: u16,
    is_primary: bool,
    format: OutputFormat,
) -> String {
    let slug = path_slug(template);
    let ext = format.extension();
    if is_primary {
        format!("{}_{}.{}", method, slug, ext)
    } else {
        format!("{}_{}_{}.{}{}", method, slug, status, ext, DISABLED_SUFFIX)
    }
}

/// One response the importer will emit a file for.
struct GeneratedResponse {
    status: u16,
    /// True for OpenAPI's `default` catch-all key, which usually documents an
    /// *error* shape. It maps to `default_status` for its filename but must
    /// not outrank an explicitly declared success response.
    is_catch_all: bool,
    body: Value,
}

/// Pick which of an operation's responses becomes the single live mock.
///
/// The `--status` code wins if the operation declares it; otherwise the
/// lowest declared 2xx, then the lowest declared status. The `default`
/// catch-all is only chosen when it is all the operation has — a spec that
/// documents `201` alongside a `default` error means the 201, and serving the
/// error shape by default would be actively misleading.
fn choose_primary(responses: &[GeneratedResponse], default_status: u16) -> usize {
    let declared = |response: &&GeneratedResponse| !response.is_catch_all;

    let exact = responses
        .iter()
        .position(|response| declared(&response) && response.status == default_status);
    if let Some(index) = exact {
        return index;
    }

    let lowest = |filter: fn(u16) -> bool| {
        responses
            .iter()
            .enumerate()
            .filter(|(_, response)| declared(response) && filter(response.status))
            .min_by_key(|(_, response)| response.status)
            .map(|(index, _)| index)
    };

    lowest(|status| (200..300).contains(&status))
        .or_else(|| lowest(|_| true))
        .unwrap_or(0)
}

/// Split a generated filename into its stem and extension, treating
/// `.json.disabled` / `.yaml.disabled` as a single extension.
fn split_extension(filename: &str) -> (&str, &str) {
    // `get_a_404.json.disabled` -> ("get_a_404", ".json.disabled")
    if let Some(rest) = filename.strip_suffix(DISABLED_SUFFIX) {
        for ext in [".json", ".yaml"] {
            if let Some(stem) = rest.strip_suffix(ext) {
                return (stem, &filename[stem.len()..]);
            }
        }
    }
    for ext in [".json", ".yaml"] {
        if let Some(stem) = filename.strip_suffix(ext) {
            return (stem, ext);
        }
    }
    (filename, "")
}

/// Collapse a path template into a filesystem-safe slug: `/users/{id}` ->
/// `users_id`.
fn path_slug(template: &str) -> String {
    let mut slug = String::with_capacity(template.len());
    for character in template.chars() {
        let lowered = character.to_ascii_lowercase();
        if lowered.is_ascii_alphanumeric() {
            slug.push(lowered);
        } else if !slug.ends_with('_') {
            slug.push('_');
        }
    }
    let trimmed = slug.trim_matches('_');
    if trimmed.is_empty() {
        "root".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Keep generated filenames distinct when two operations slug identically
/// (`/users/{id}` and `/users/{userId}` both slug to `users_id`-ish shapes).
fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let (stem, extension) = split_extension(base);
    for suffix in 2.. {
        let candidate = format!("{}_{}{}", stem, suffix, extension);
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("suffix search is unbounded")
}

fn dir_has_entries(dir: &Path) -> Result<bool, ImportError> {
    match fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(ImportError::Io(format!(
            "cannot inspect output directory '{}': {}",
            dir.display(),
            e
        ))),
    }
}

/// Convert a parsed YAML document to `serde_json::Value`.
///
/// Deserializing YAML straight into `serde_json::Value` would reject the very
/// common unquoted `200:` response key, since JSON maps only take string keys.
/// Converting by hand lets scalar keys be stringified the way the OpenAPI spec
/// means them.
fn yaml_to_json(value: serde_yaml::Value) -> Result<Value, ImportError> {
    Ok(match value {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(flag) => Value::Bool(flag),
        serde_yaml::Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Value::from(int)
            } else if let Some(uint) = number.as_u64() {
                Value::from(uint)
            } else {
                number
                    .as_f64()
                    .and_then(serde_json::Number::from_f64)
                    .map_or(Value::Null, Value::Number)
            }
        }
        serde_yaml::Value::String(text) => Value::String(text),
        serde_yaml::Value::Sequence(items) => Value::Array(
            items
                .into_iter()
                .map(yaml_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        serde_yaml::Value::Mapping(mapping) => {
            let mut object = Map::new();
            for (key, value) in mapping {
                object.insert(yaml_key(&key)?, yaml_to_json(value)?);
            }
            Value::Object(object)
        }
        // `!!tag`ged nodes: the tag carries no meaning for us, the value does.
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(tagged.value)?,
    })
}

fn yaml_key(key: &serde_yaml::Value) -> Result<String, ImportError> {
    match key {
        serde_yaml::Value::String(text) => Ok(text.clone()),
        serde_yaml::Value::Number(number) => Ok(number.to_string()),
        serde_yaml::Value::Bool(flag) => Ok(flag.to_string()),
        serde_yaml::Value::Null => Ok("null".to_string()),
        _ => Err(ImportError::Invalid(
            "spec contains a non-scalar mapping key, which OpenAPI does not allow".to_string(),
        )),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A spec exercising the shapes the importer has to survive: path
    /// parameters, several response codes, `$ref`-based schemas, a named
    /// `examples` map, and an operation with no example at all.
    const FIXTURE: &str = r#"
openapi: 3.0.3
info:
  title: Fixture API
  version: "1.0.0"
paths:
  /users:
    get:
      responses:
        200:
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: '#/components/schemas/User'
    post:
      responses:
        '201':
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/User'
        '400':
          content:
            application/json:
              example: {"error": "bad request"}
  /users/{id}:
    get:
      responses:
        '200':
          content:
            application/json:
              example: {"id": 1, "name": "Alice"}
        '404':
          content:
            application/json:
              examples:
                missing:
                  value: {"error": "not found"}
    delete:
      responses:
        '204': {}
components:
  schemas:
    User:
      type: object
      properties:
        id:
          type: integer
        name:
          type: string
        active:
          type: boolean
"#;

    fn fixture() -> OpenApiSpec {
        OpenApiSpec::parse(FIXTURE).expect("fixture spec should parse")
    }

    fn generate(spec: &OpenApiSpec) -> Vec<(String, MockConfig)> {
        generate_mocks(
            spec,
            DEFAULT_STATUS,
            PathStyle::Colon,
            OutputFormat::Json,
            "",
        )
    }

    fn find<'a>(mocks: &'a [(String, MockConfig)], name: &str) -> &'a MockConfig {
        mocks
            .iter()
            .find(|(filename, _)| filename == name)
            .map(|(_, config)| config)
            .unwrap_or_else(|| {
                panic!(
                    "no mock named '{}'; generated: {:?}",
                    name,
                    mocks.iter().map(|(n, _)| n).collect::<Vec<_>>()
                )
            })
    }

    // ------------------------------------------------------------------
    // Parsing
    // ------------------------------------------------------------------

    #[test]
    fn test_parses_yaml_spec() {
        let spec = fixture();
        assert!(spec.paths().contains_key("/users/{id}"));
    }

    #[test]
    fn test_parses_json_spec() {
        let source = json!({
            "openapi": "3.1.0",
            "paths": {"/ping": {"get": {"responses": {"200": {}}}}}
        })
        .to_string();

        let spec = OpenApiSpec::parse(&source).unwrap();
        let mocks = generate(&spec);

        assert_eq!(mocks.len(), 1);
        assert_eq!(mocks[0].0, "get_ping.json");
    }

    #[test]
    fn test_unquoted_yaml_status_keys_are_accepted() {
        // `200:` without quotes is a YAML integer key — extremely common in
        // hand-written specs, and it must not break the import.
        let mocks = generate(&fixture());
        assert_eq!(find(&mocks, "get_users.json").status, 200);
    }

    #[test]
    fn test_malformed_yaml_reports_error_without_panicking() {
        let error = OpenApiSpec::parse("openapi: 3.0.0\npaths:\n  - [unclosed").unwrap_err();
        assert!(matches!(error, ImportError::Parse(_)), "got {:?}", error);
    }

    #[test]
    fn test_malformed_json_reports_error_without_panicking() {
        let error = OpenApiSpec::parse("{\"openapi\": ").unwrap_err();
        assert!(matches!(error, ImportError::Parse(_)), "got {:?}", error);
    }

    #[test]
    fn test_non_object_root_is_rejected() {
        let error = OpenApiSpec::parse("- just\n- a list\n").unwrap_err();
        assert!(matches!(error, ImportError::Invalid(_)), "got {:?}", error);
    }

    #[test]
    fn test_missing_openapi_version_is_rejected() {
        let error = OpenApiSpec::parse("paths: {}\n").unwrap_err();
        match error {
            ImportError::Invalid(message) => assert!(message.contains("openapi")),
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn test_swagger_2_is_rejected_with_a_clear_message() {
        let error = OpenApiSpec::parse("swagger: \"2.0\"\npaths: {}\n").unwrap_err();
        match error {
            ImportError::Invalid(message) => assert!(message.contains("OpenAPI 3.x")),
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn test_missing_paths_is_rejected() {
        let error = OpenApiSpec::parse("openapi: 3.0.0\ninfo: {}\n").unwrap_err();
        match error {
            ImportError::Invalid(message) => assert!(message.contains("paths")),
            other => panic!("got {:?}", other),
        }
    }

    // ------------------------------------------------------------------
    // Generation from examples
    // ------------------------------------------------------------------

    #[test]
    fn test_example_based_generation() {
        let mocks = generate(&fixture());
        let mock = find(&mocks, "get_users_id.json");

        assert_eq!(mock.method, "GET");
        assert_eq!(mock.path, "/users/:id");
        assert_eq!(mock.status, 200);
        assert_eq!(mock.response, json!({"id": 1, "name": "Alice"}));
    }

    #[test]
    fn test_named_examples_map_is_used() {
        let mocks = generate(&fixture());
        assert_eq!(
            find(&mocks, "get_users_id_404.json.disabled").response,
            json!({"error": "not found"})
        );
    }

    #[test]
    fn test_path_parameters_become_colon_syntax_by_default() {
        let mocks = generate(&fixture());
        assert_eq!(find(&mocks, "delete_users_id.json").path, "/users/:id");
    }

    #[test]
    fn test_brace_params_option_passes_the_template_through() {
        let mocks = generate_mocks(
            &fixture(),
            DEFAULT_STATUS,
            PathStyle::Brace,
            OutputFormat::Json,
            "",
        );
        assert_eq!(find(&mocks, "get_users_id.json").path, "/users/{id}");
    }

    #[test]
    fn test_multiple_statuses_produce_separate_suffixed_files() {
        let mocks = generate(&fixture());
        let names: Vec<&str> = mocks.iter().map(|(name, _)| name.as_str()).collect();

        // The primary response is live; the alternatives are suffixed and inert.
        assert!(names.contains(&"get_users_id.json"));
        assert!(names.contains(&"get_users_id_404.json.disabled"));
        // POST /users declares no 200, so its lowest 2xx (201) is primary.
        assert!(names.contains(&"post_users.json"));
        assert!(names.contains(&"post_users_400.json.disabled"));

        assert_eq!(find(&mocks, "post_users.json").status, 201);
        assert_eq!(find(&mocks, "post_users_400.json.disabled").status, 400);
    }

    #[test]
    fn test_status_option_picks_the_primary_when_the_operation_declares_it() {
        let mocks = generate_mocks(&fixture(), 201, PathStyle::Colon, OutputFormat::Json, "");

        // POST /users declares 201, so --status 201 makes it the live file.
        assert_eq!(find(&mocks, "post_users.json").status, 201);
        // GET /users/{id} has no 201; it falls back to its lowest 2xx (200)
        // rather than leaving the route with nothing live.
        assert_eq!(find(&mocks, "get_users_id.json").status, 200);
    }

    /// OpenAPI's `default` key usually documents an *error* shape. It must not
    /// beat a declared success response to the live slot just because it maps
    /// onto `--status`.
    #[test]
    fn test_catch_all_default_does_not_outrank_a_declared_success() {
        let spec = OpenApiSpec::parse(
            r#"
openapi: 3.0.0
paths:
  /pets:
    post:
      responses:
        '201':
          content:
            application/json:
              example: {"id": 1}
        default:
          content:
            application/json:
              example: {"error": "boom"}
"#,
        )
        .unwrap();

        let mocks = generate(&spec);

        let primary = find(&mocks, "post_pets.json");
        assert_eq!(primary.status, 201);
        assert_eq!(primary.response, json!({"id": 1}));
        assert_eq!(
            find(&mocks, "post_pets_200.json.disabled").response,
            json!({"error": "boom"})
        );
    }

    /// ...but when `default` is all there is, it is the live one.
    #[test]
    fn test_lone_catch_all_default_becomes_the_primary() {
        let spec = OpenApiSpec::parse(
            "openapi: 3.0.0\npaths:\n  /a:\n    get:\n      responses:\n        default:\n          content:\n            application/json:\n              example: {\"ok\": true}\n",
        )
        .unwrap();

        let mocks = generate(&spec);
        assert_eq!(mocks.len(), 1);
        assert_eq!(mocks[0].0, "get_a.json");
        assert_eq!(mocks[0].1.response, json!({"ok": true}));
    }

    /// An operation documenting only errors still gets a live mock — the
    /// lowest declared status — rather than an all-disabled route.
    #[test]
    fn test_operation_with_no_success_response_still_has_a_live_mock() {
        let spec = OpenApiSpec::parse(
            "openapi: 3.0.0\npaths:\n  /a:\n    get:\n      responses:\n        '500': {}\n        '404': {}\n",
        )
        .unwrap();

        let mocks = generate(&spec);
        assert_eq!(find(&mocks, "get_a.json").status, 404);
        assert_eq!(find(&mocks, "get_a_500.json.disabled").status, 500);
    }

    #[test]
    fn test_response_without_content_yields_an_empty_object() {
        let mocks = generate(&fixture());
        let mock = find(&mocks, "delete_users_id.json");
        assert_eq!(mock.status, 204);
        assert_eq!(mock.response, json!({}));
    }

    #[test]
    fn test_operation_without_responses_still_generates_a_mock() {
        let spec = OpenApiSpec::parse(
            "openapi: 3.0.0\npaths:\n  /ping:\n    get:\n      summary: no responses\n",
        )
        .unwrap();
        let mocks = generate(&spec);

        assert_eq!(mocks.len(), 1);
        assert_eq!(mocks[0].1.status, 200);
        assert_eq!(mocks[0].1.response, json!({}));
    }

    #[test]
    fn test_generation_is_deterministic() {
        let spec = fixture();
        let render = |mocks: Vec<(String, MockConfig)>| {
            mocks
                .into_iter()
                .map(|(name, config)| (name, serde_json::to_value(config).unwrap()))
                .collect::<Vec<_>>()
        };
        assert_eq!(render(generate(&spec)), render(generate(&spec)));
    }

    // ------------------------------------------------------------------
    // Schema stubs
    // ------------------------------------------------------------------

    #[test]
    fn test_missing_example_falls_back_to_ref_resolved_schema_stub() {
        let mocks = generate(&fixture());
        assert_eq!(
            find(&mocks, "post_users.json").response,
            json!({"id": 0, "name": "string", "active": false})
        );
    }

    #[test]
    fn test_array_schema_stubs_one_sample_element() {
        let mocks = generate(&fixture());
        assert_eq!(
            find(&mocks, "get_users.json").response,
            json!([{"id": 0, "name": "string", "active": false}])
        );
    }

    fn stub_of(schema: serde_json::Value) -> Value {
        let spec = OpenApiSpec::parse(
            &json!({"openapi": "3.0.0", "paths": {}, "components": {}}).to_string(),
        )
        .unwrap();
        spec.stub_from_schema(&schema, &mut Vec::new(), 0)
    }

    #[test]
    fn test_scalar_type_stubs() {
        assert_eq!(stub_of(json!({"type": "string"})), json!("string"));
        assert_eq!(stub_of(json!({"type": "integer"})), json!(0));
        assert_eq!(stub_of(json!({"type": "number"})), json!(0.0));
        assert_eq!(stub_of(json!({"type": "boolean"})), json!(false));
        assert_eq!(stub_of(json!({"type": "null"})), json!(null));
        assert_eq!(stub_of(json!({"type": "object"})), json!({}));
        assert_eq!(stub_of(json!({"type": "array"})), json!([]));
    }

    #[test]
    fn test_string_formats_get_plausible_stubs() {
        assert_eq!(
            stub_of(json!({"type": "string", "format": "date-time"})),
            json!("2024-01-01T00:00:00Z")
        );
        assert_eq!(
            stub_of(json!({"type": "string", "format": "email"})),
            json!("user@example.com")
        );
        assert_eq!(
            stub_of(json!({"type": "string", "format": "unheard-of"})),
            json!("string")
        );
    }

    #[test]
    fn test_nested_object_stubs_recurse() {
        let stub = stub_of(json!({
            "type": "object",
            "properties": {
                "meta": {
                    "type": "object",
                    "properties": {"count": {"type": "integer"}}
                },
                "tags": {"type": "array", "items": {"type": "string"}}
            }
        }));

        assert_eq!(stub, json!({"meta": {"count": 0}, "tags": ["string"]}));
    }

    #[test]
    fn test_schema_example_default_and_enum_win_over_stubs() {
        assert_eq!(
            stub_of(json!({"type": "string", "example": "hello"})),
            json!("hello")
        );
        assert_eq!(stub_of(json!({"type": "integer", "default": 7})), json!(7));
        assert_eq!(
            stub_of(json!({"type": "string", "enum": ["active", "banned"]})),
            json!("active")
        );
    }

    #[test]
    fn test_all_of_merges_and_one_of_takes_the_first_variant() {
        assert_eq!(
            stub_of(json!({"allOf": [
                {"type": "object", "properties": {"id": {"type": "integer"}}},
                {"type": "object", "properties": {"name": {"type": "string"}}}
            ]})),
            json!({"id": 0, "name": "string"})
        );
        assert_eq!(
            stub_of(json!({"oneOf": [{"type": "boolean"}, {"type": "string"}]})),
            json!(false)
        );
    }

    #[test]
    fn test_nullable_type_array_uses_the_non_null_type() {
        assert_eq!(
            stub_of(json!({"type": ["string", "null"]})),
            json!("string")
        );
    }

    #[test]
    fn test_properties_without_a_declared_type_are_treated_as_an_object() {
        assert_eq!(
            stub_of(json!({"properties": {"id": {"type": "integer"}}})),
            json!({"id": 0})
        );
    }

    // ------------------------------------------------------------------
    // $ref edge cases
    // ------------------------------------------------------------------

    #[test]
    fn test_shared_response_and_path_item_refs_resolve() {
        let spec = OpenApiSpec::parse(
            r#"
openapi: 3.0.0
paths:
  /a:
    get:
      responses:
        '200':
          $ref: '#/components/responses/Ok'
components:
  responses:
    Ok:
      content:
        application/json:
          example: {"ok": true}
"#,
        )
        .unwrap();

        let mocks = generate(&spec);
        assert_eq!(find(&mocks, "get_a.json").response, json!({"ok": true}));
    }

    #[test]
    fn test_circular_ref_degrades_to_empty_object() {
        let spec = OpenApiSpec::parse(
            r#"
openapi: 3.0.0
paths:
  /nodes:
    get:
      responses:
        '200':
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Node'
components:
  schemas:
    Node:
      type: object
      properties:
        name:
          type: string
        child:
          $ref: '#/components/schemas/Node'
"#,
        )
        .unwrap();

        let mocks = generate(&spec);
        assert_eq!(
            find(&mocks, "get_nodes.json").response,
            json!({"name": "string", "child": {}})
        );
    }

    #[test]
    fn test_unresolvable_and_external_refs_degrade_to_empty_object() {
        for reference in ["#/components/schemas/Missing", "./other.yaml#/User"] {
            let spec = OpenApiSpec::parse(&format!(
                "openapi: 3.0.0\npaths:\n  /a:\n    get:\n      responses:\n        '200':\n          content:\n            application/json:\n              schema:\n                $ref: '{}'\n",
                reference
            ))
            .unwrap();

            let mocks = generate(&spec);
            assert_eq!(
                find(&mocks, "get_a.json").response,
                json!({}),
                "ref {}",
                reference
            );
        }
    }

    #[test]
    fn test_mutually_recursive_response_refs_do_not_hang() {
        let spec = OpenApiSpec::parse(
            r#"
openapi: 3.0.0
paths:
  /a:
    get:
      responses:
        '200':
          $ref: '#/components/responses/A'
components:
  responses:
    A:
      $ref: '#/components/responses/B'
    B:
      $ref: '#/components/responses/A'
"#,
        )
        .unwrap();

        assert_eq!(find(&generate(&spec), "get_a.json").response, json!({}));
    }

    // ------------------------------------------------------------------
    // Filenames and paths
    // ------------------------------------------------------------------

    #[test]
    fn test_path_slug() {
        assert_eq!(path_slug("/users/{id}"), "users_id");
        assert_eq!(path_slug("/api/v1/user-profiles"), "api_v1_user_profiles");
        assert_eq!(path_slug("/"), "root");
    }

    #[test]
    fn test_translate_path() {
        assert_eq!(
            translate_path("/users/{id}/posts/{postId}", PathStyle::Colon),
            "/users/:id/posts/:postId"
        );
        assert_eq!(
            translate_path("/users/{id}", PathStyle::Brace),
            "/users/{id}"
        );
        // `{}` is a literal to the matcher, so it stays one here too.
        assert_eq!(translate_path("/a/{}", PathStyle::Colon), "/a/{}");
    }

    // ------------------------------------------------------------------
    // Base paths from `servers` (#118)
    // ------------------------------------------------------------------

    const SERVERS_FIXTURE: &str = r#"
openapi: 3.0.3
info:
  title: Versioned API
  version: "1.0.0"
servers:
  - url: https://api.example.com/v1
paths:
  /users/{id}:
    get:
      responses:
        '200':
          content:
            application/json:
              example: {"id": 1, "name": "Ada"}
"#;

    fn generate_with_base(spec: &OpenApiSpec, base: &str) -> Vec<(String, MockConfig)> {
        generate_mocks(
            spec,
            DEFAULT_STATUS,
            PathStyle::Colon,
            OutputFormat::Json,
            base,
        )
    }

    fn generate_servers(yaml: &str) -> Vec<(String, MockConfig)> {
        // Mirrors how `run_import` wires the pieces: derive the base from
        // the spec's `servers` (nothing → empty), then hand it to
        // `generate_mocks`, which is a pure function of its arguments.
        let spec = OpenApiSpec::parse(yaml).expect("spec should parse");
        let base = spec.top_level_base_path().unwrap_or_default();
        generate_with_base(&spec, &base)
    }

    #[test]
    fn test_servers_url_path_becomes_the_base_path() {
        let mocks = generate_servers(SERVERS_FIXTURE);
        assert_eq!(find(&mocks, "get_users_id.json").path, "/v1/users/:id");
    }

    #[test]
    fn test_a_server_without_a_path_changes_nothing() {
        let yaml = SERVERS_FIXTURE.replace(
            "servers:\n  - url: https://api.example.com/v1",
            "servers:\n  - url: https://api.example.com",
        );
        let with_pathless_server = generate_servers(&yaml);

        // The claim is precisely that a server with no path component
        // contributes nothing to any generated path.
        for (name, mock) in &with_pathless_server {
            assert!(
                !mock.path.contains("/v1"),
                "{} must not grow a base path: {}",
                name,
                mock.path
            );
        }
        assert_eq!(
            find(&with_pathless_server, "get_users_id.json").path,
            "/users/:id"
        );
    }

    #[test]
    fn test_trailing_slash_on_the_server_url_is_trimmed() {
        let yaml = SERVERS_FIXTURE.replace(
            "- url: https://api.example.com/v1",
            "- url: https://api.example.com/v1/",
        );
        assert_eq!(
            find(&generate_servers(&yaml), "get_users_id.json").path,
            "/v1/users/:id"
        );
    }

    #[test]
    fn test_a_relative_server_url_uses_its_path() {
        let yaml = SERVERS_FIXTURE.replace("- url: https://api.example.com/v1", "- url: /v1");
        assert_eq!(
            find(&generate_servers(&yaml), "get_users_id.json").path,
            "/v1/users/:id"
        );
    }

    #[test]
    fn test_server_variables_resolve_from_their_defaults() {
        let yaml = r#"
openapi: 3.0.3
info: {title: Variables, version: "1.0"}
servers:
  - url: "https://{host}/v1"
    variables:
      host:
        default: api.example.com
paths:
  /users:
    get:
      responses:
        '200':
          content:
            application/json:
              example: {"ok": true}
"#;
        assert_eq!(
            find(&generate_servers(yaml), "get_users.json").path,
            "/v1/users"
        );
    }

    #[test]
    fn test_a_path_variable_without_a_default_is_dropped_not_emitted() {
        // `tenant` has no default, so `{tenant}` must not land in the mock's
        // path — the empty segments it leaves collapse away. The warning is
        // the point of the behavior; the path assertion pins the mechanics.
        let yaml = SERVERS_FIXTURE.replace(
            "- url: https://api.example.com/v1",
            "- url: https://api.example.com/{tenant}/v1",
        );
        assert_eq!(
            find(&generate_servers(&yaml), "get_users_id.json").path,
            "/v1/users/:id"
        );
    }

    #[test]
    fn test_a_host_variable_without_a_default_does_not_break_the_path() {
        // The variable sits in the host, which a mock ignores anyway — the
        // path component must still be extracted, warning or not.
        let yaml = SERVERS_FIXTURE.replace(
            "- url: https://api.example.com/v1",
            "- url: https://{host}/v1",
        );
        assert_eq!(
            find(&generate_servers(&yaml), "get_users_id.json").path,
            "/v1/users/:id"
        );
    }

    #[test]
    fn test_multiple_servers_use_the_first() {
        let yaml = SERVERS_FIXTURE.replace(
            "servers:\n  - url: https://api.example.com/v1",
            "servers:\n  - url: https://api.example.com/v1\n  - url: https://staging.example.com/v2",
        );
        // First entry wins; the warning names `--base-path` for the rest.
        assert_eq!(
            find(&generate_servers(&yaml), "get_users_id.json").path,
            "/v1/users/:id"
        );
    }

    #[test]
    fn test_path_level_servers_override_the_global_for_that_path() {
        let yaml = r#"
openapi: 3.0.3
info: {title: Mixed, version: "1.0"}
servers:
  - url: https://api.example.com/v1
paths:
  /users:
    get:
      responses:
        '200':
          content:
            application/json:
              example: {"ok": true}
  /admin/status:
    servers:
      - url: https://api.example.com/v2
    get:
      responses:
        '200':
          content:
            application/json:
              example: {"ok": true}
"#;
        let mocks = generate_servers(yaml);
        assert_eq!(find(&mocks, "get_users.json").path, "/v1/users");
        // OpenAPI semantics: a non-empty path-level `servers` replaces the
        // global list, not augments it.
        assert_eq!(
            find(&mocks, "get_admin_status.json").path,
            "/v2/admin/status"
        );
    }

    #[test]
    fn test_an_empty_path_level_servers_list_falls_back_to_the_global() {
        let yaml = r#"
openapi: 3.0.3
info: {title: Fallback, version: "1.0"}
servers:
  - url: https://api.example.com/v1
paths:
  /users:
    get:
      servers: []
      responses:
        '200':
          content:
            application/json:
              example: {"ok": true}
"#;
        // `servers: []` is not an override — the document-level list applies.
        assert_eq!(
            find(&generate_servers(yaml), "get_users.json").path,
            "/v1/users"
        );
    }

    #[test]
    fn test_operation_level_servers_override_the_path_level() {
        let yaml = r#"
openapi: 3.0.3
info: {title: Operation level, version: "1.0"}
servers:
  - url: https://api.example.com/v1
paths:
  /admin/status:
    servers:
      - url: https://api.example.com/v2
    get:
      responses:
        '200':
          content:
            application/json:
              example: {"ok": true}
    post:
      servers:
        - url: https://api.example.com/v3
      responses:
        '201': {}
"#;
        let mocks = generate_servers(yaml);
        // Most specific non-empty list wins: operation over path item.
        assert_eq!(
            find(&mocks, "get_admin_status.json").path,
            "/v2/admin/status"
        );
        assert_eq!(
            find(&mocks, "post_admin_status.json").path,
            "/v3/admin/status"
        );
    }

    #[test]
    fn test_base_filename_ignores_the_base_path() {
        // 40 operations under a base path would otherwise lengthen every
        // generated file name for no benefit — the prefix lives in `path`
        // only. (The fixture's names are the same as the no-servers case.)
        let mocks = generate_servers(SERVERS_FIXTURE);
        assert!(mocks.iter().any(|(name, _)| name == "get_users_id.json"));
    }

    #[test]
    fn test_explicit_base_path_beats_the_spec() {
        let base = "/api";
        let mocks = generate_with_base(&OpenApiSpec::parse(SERVERS_FIXTURE).unwrap(), base);
        assert_eq!(find(&mocks, "get_users_id.json").path, "/api/users/:id");
    }

    #[test]
    fn test_the_document_root_template_sits_directly_under_the_base() {
        let yaml = r#"
openapi: 3.0.3
info: {title: Root, version: "1.0"}
servers:
  - url: https://api.example.com/v1
paths:
  /:
    get:
      responses:
        '200':
          content:
            application/json:
              example: {"ok": true}
"#;
        // `GET /` on a /v1 deployment is `GET /v1` — not a doubled slash.
        let mocks = generate_servers(yaml);
        assert!(mocks.iter().any(|(_, m)| m.path == "/v1"));
    }

    #[test]
    fn test_base_path_brace_style_keeps_the_prefix_and_template() {
        let mocks = generate_mocks(
            &OpenApiSpec::parse(SERVERS_FIXTURE).unwrap(),
            DEFAULT_STATUS,
            PathStyle::Brace,
            OutputFormat::Json,
            "/v1",
        );
        assert_eq!(find(&mocks, "get_users_id.json").path, "/v1/users/{id}");
    }

    #[test]
    fn test_url_path_component_extracts_the_path_only() {
        assert_eq!(
            server_path_component("https://api.example.com/v1"),
            Some("/v1".into())
        );
        assert_eq!(server_path_component("https://api.example.com"), None);
        assert_eq!(server_path_component("https://api.example.com/"), None);
        assert_eq!(
            server_path_component("http://localhost:8080/api/v2"),
            Some("/api/v2".into())
        );
        assert_eq!(server_path_component("/v1"), Some("/v1".into()));
        assert_eq!(
            server_path_component("https://api.example.com/v1/"),
            Some("/v1".into())
        );
        assert_eq!(
            server_path_component("https://api.example.com/a//b"),
            Some("/a/b".into())
        );
        assert_eq!(server_path_component(""), None);
        assert_eq!(
            server_path_component("https://{host}:{port}/v1"),
            Some("/v1".into())
        );
    }

    #[test]
    fn test_base_path_values_are_normalized() {
        assert_eq!(
            parse_args(&args(&["spec.yaml", "--base-path", "/api"]))
                .unwrap()
                .base_path,
            Some("/api".to_string())
        );
        assert_eq!(
            parse_args(&args(&["spec.yaml", "--base-path", "api"]))
                .unwrap()
                .base_path,
            Some("/api".to_string())
        );
        assert_eq!(
            parse_args(&args(&["spec.yaml", "--base-path", "/api/"]))
                .unwrap()
                .base_path,
            Some("/api".to_string())
        );
        assert_eq!(
            parse_args(&args(&["spec.yaml", "--base-path", "//api"]))
                .unwrap()
                .base_path,
            Some("/api".to_string())
        );
        // An empty value is the explicit drop, not a missing one — the flag
        // is present, and `None` means "derive from the spec" instead.
        assert_eq!(
            parse_args(&args(&["spec.yaml", "--base-path", ""]))
                .unwrap()
                .base_path,
            Some(String::new())
        );
        assert_eq!(
            parse_args(&args(&["spec.yaml", "--base-path", "/"]))
                .unwrap()
                .base_path,
            Some(String::new())
        );
        assert_eq!(parse_args(&args(&["spec.yaml"])).unwrap().base_path, None);
    }

    #[test]
    fn test_run_import_applies_the_base_path() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("spec.yaml");
        fs::write(&spec_path, SERVERS_FIXTURE).unwrap();
        let out = dir.path().join("generated");

        let (written, base_path) = run_import(&args(&[
            spec_path.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ]))
        .unwrap();

        assert_eq!(base_path, "/v1");
        assert!(out.join("get_users_id.json").exists());
        let on_disk = fs::read_to_string(out.join("get_users_id.json")).unwrap();
        assert!(on_disk.contains("\"/v1/users/:id\""));
        let _ = written;
    }

    #[test]
    fn test_run_import_base_path_flag_overrides_the_spec() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("spec.yaml");
        fs::write(&spec_path, SERVERS_FIXTURE).unwrap();
        let out = dir.path().join("generated");

        let (_, base_path) = run_import(&args(&[
            spec_path.to_str().unwrap(),
            "--base-path",
            "/gateway",
            "--out",
            out.to_str().unwrap(),
        ]))
        .unwrap();

        assert_eq!(base_path, "/gateway");
        let on_disk = fs::read_to_string(out.join("get_users_id.json")).unwrap();
        assert!(on_disk.contains("\"/gateway/users/:id\""));
    }

    #[test]
    fn test_run_import_base_path_empty_flag_drops_the_spec_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("spec.yaml");
        fs::write(&spec_path, SERVERS_FIXTURE).unwrap();
        let out = dir.path().join("generated");

        let (_, base_path) = run_import(&args(&[
            spec_path.to_str().unwrap(),
            // A gateway strips /v1 in front of the mock server, so the
            // operator asks for no prefix at all.
            "--base-path",
            "",
            "--out",
            out.to_str().unwrap(),
        ]))
        .unwrap();

        assert_eq!(base_path, "");
        let on_disk = fs::read_to_string(out.join("get_users_id.json")).unwrap();
        assert!(on_disk.contains("\"/users/:id\""));
    }

    #[test]
    fn test_status_code_keys() {
        assert_eq!(status_code("200", 200), Some(200));
        assert_eq!(status_code("default", 418), Some(418));
        assert_eq!(status_code("4XX", 200), Some(400));
        assert_eq!(status_code("garbage", 200), None);
        assert_eq!(status_code("999", 200), None);
    }

    #[test]
    fn test_colliding_filenames_get_numeric_suffixes() {
        // Both templates slug to `users_id`, and both are 200 responses.
        let spec = OpenApiSpec::parse(
            r#"
openapi: 3.0.0
paths:
  /users/{id}:
    get:
      responses:
        '200': {}
  /users/[id]:
    get:
      responses:
        '200': {}
"#,
        )
        .unwrap();

        let names: Vec<String> = generate(&spec).into_iter().map(|(name, _)| name).collect();
        assert!(
            names.contains(&"get_users_id.json".to_string()),
            "{:?}",
            names
        );
        assert!(
            names.contains(&"get_users_id_2.json".to_string()),
            "{:?}",
            names
        );
    }

    #[test]
    fn test_non_path_keys_in_paths_are_skipped() {
        let spec = OpenApiSpec::parse(
            "openapi: 3.0.0\npaths:\n  x-internal: true\n  /ok:\n    get:\n      responses:\n        '200': {}\n",
        )
        .unwrap();

        let mocks = generate(&spec);
        assert_eq!(mocks.len(), 1);
        assert_eq!(mocks[0].0, "get_ok.json");
    }

    // ------------------------------------------------------------------
    // Argument parsing
    // ------------------------------------------------------------------

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn test_parse_args_defaults() {
        let options = parse_args(&args(&["spec.yaml"])).unwrap();

        assert_eq!(options.spec_path, PathBuf::from("spec.yaml"));
        assert_eq!(options.out_dir, PathBuf::from(DEFAULT_OUT_DIR));
        assert_eq!(options.default_status, 200);
        assert!(!options.force);
        assert_eq!(options.path_style, PathStyle::Colon);
        assert_eq!(options.format, OutputFormat::Json);
    }

    #[test]
    fn test_parse_args_all_options() {
        let options = parse_args(&args(&[
            "--out",
            "./out",
            "--status",
            "201",
            "--force",
            "--brace-params",
            "--format",
            "yaml",
            "spec.json",
        ]))
        .unwrap();

        assert_eq!(options.spec_path, PathBuf::from("spec.json"));
        assert_eq!(options.out_dir, PathBuf::from("./out"));
        assert_eq!(options.default_status, 201);
        assert!(options.force);
        assert_eq!(options.path_style, PathStyle::Brace);
        assert_eq!(options.format, OutputFormat::Yaml);
    }

    #[test]
    fn test_format_option_accepts_yml_as_an_alias_for_yaml() {
        let options = parse_args(&args(&["--format", "yml", "spec.json"])).unwrap();
        assert_eq!(options.format, OutputFormat::Yaml);
    }

    #[test]
    fn test_format_option_is_case_insensitive() {
        let options = parse_args(&args(&["--format", "YAML", "spec.json"])).unwrap();
        assert_eq!(options.format, OutputFormat::Yaml);
    }

    #[test]
    fn test_parse_args_rejects_bad_input() {
        for bad in [
            vec!["--out"],
            vec!["--status", "not-a-number", "spec.yaml"],
            vec!["--format", "toml", "spec.yaml"],
            vec!["--format"],
            vec!["--nope", "spec.yaml"],
            vec!["a.yaml", "b.yaml"],
            vec![],
        ] {
            let error = parse_args(&args(&bad)).unwrap_err();
            assert!(
                matches!(error, ImportError::Usage(_)),
                "{:?} -> {:?}",
                bad,
                error
            );
        }
    }

    #[test]
    fn test_parse_args_help() {
        assert!(matches!(
            parse_args(&args(&["--help"])),
            Err(ImportError::Usage(message)) if message.is_empty()
        ));
    }

    // ------------------------------------------------------------------
    // Writing output
    // ------------------------------------------------------------------

    #[test]
    fn test_write_mocks_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("generated");
        let mocks = generate(&fixture());

        let written = write_mocks(&mocks, &out, false, OutputFormat::Json).unwrap();

        assert_eq!(written.len(), mocks.len());
        let contents = fs::read_to_string(out.join("get_users_id.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed["path"], "/users/:id");
        assert!(contents.ends_with('\n'));
    }

    #[test]
    fn test_format_yaml_writes_yaml_files_the_loader_can_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("generated");
        let mocks = generate_mocks(
            &fixture(),
            DEFAULT_STATUS,
            PathStyle::Colon,
            OutputFormat::Yaml,
            "",
        );

        let written = write_mocks(&mocks, &out, false, OutputFormat::Yaml).unwrap();

        assert_eq!(written.len(), mocks.len());
        let path = out.join("get_users_id.yaml");
        assert!(path.exists(), "expected a .yaml file, found: {:?}", written);

        let contents = fs::read_to_string(&path).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(parsed["path"], serde_yaml::Value::from("/users/:id"));

        // What the loader itself reads back must round-trip identically to
        // the JSON output for the same spec.
        let loaded = crate::loader::load_mocks_map(out.to_str().unwrap());
        assert_eq!(loaded.errors.len(), 0);
        assert!(loaded.mocks.contains_key("GET:/users/:id"));
    }

    #[test]
    fn test_format_yaml_names_the_disabled_alternative_status_file() {
        let mocks = generate_mocks(
            &fixture(),
            DEFAULT_STATUS,
            PathStyle::Colon,
            OutputFormat::Yaml,
            "",
        );
        let names: Vec<&str> = mocks.iter().map(|(name, _)| name.as_str()).collect();

        assert!(names.contains(&"get_users_id.yaml"));
        assert!(
            names.contains(&"get_users_id_404.yaml.disabled"),
            "{:?}",
            names
        );
    }

    #[test]
    fn test_write_mocks_refuses_non_empty_directory_without_force() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("handwritten.json"), "{}").unwrap();

        let error =
            write_mocks(&generate(&fixture()), dir.path(), false, OutputFormat::Json).unwrap_err();

        assert!(
            matches!(error, ImportError::OutputNotEmpty(_)),
            "{:?}",
            error
        );
        // The hand-written file is untouched.
        assert_eq!(
            fs::read_to_string(dir.path().join("handwritten.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn test_write_mocks_overwrites_with_force() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("get_users_id.json"), "stale").unwrap();

        write_mocks(&generate(&fixture()), dir.path(), true, OutputFormat::Json).unwrap();

        let contents = fs::read_to_string(dir.path().join("get_users_id.json")).unwrap();
        assert!(contents.contains("/users/:id"), "{}", contents);
    }

    #[test]
    fn test_run_import_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("spec.yaml");
        fs::write(&spec_path, FIXTURE).unwrap();
        let out = dir.path().join("generated");

        let (written, base_path) = run_import(&args(&[
            spec_path.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ]))
        .unwrap();

        assert!(!written.is_empty());
        assert!(out.join("get_users_id.json").exists());
        // The fixture has no `servers` block, so the applied base path is
        // none — the summary says so by being empty.
        assert_eq!(base_path, "");
    }

    #[test]
    fn test_run_import_reports_a_missing_spec_file() {
        let error = run_import(&args(&["/definitely/not/here.yaml"])).unwrap_err();
        assert!(matches!(error, ImportError::Io(_)), "{:?}", error);
    }

    // ------------------------------------------------------------------
    // The generated files must work with the rest of Mimic, unedited
    // ------------------------------------------------------------------

    #[test]
    fn test_generated_mocks_are_loadable_and_matchable() {
        let dir = tempfile::tempdir().unwrap();
        write_mocks(&generate(&fixture()), dir.path(), false, OutputFormat::Json).unwrap();

        let loaded = crate::loader::load_mocks_map(dir.path().to_str().unwrap());
        assert_eq!(loaded.errors.len(), 0, "every generated file must parse");

        // A concrete request against the generated path-parameter mock.
        let context =
            crate::matcher::RequestContext::new("GET".to_string(), "/users/42".to_string());
        let matched = crate::matcher::find_matching_mock(&context, &loaded.mocks, None)
            .expect("generated path-parameter mock should match /users/42");

        assert_eq!(matched.mock.path, "/users/:id");
        assert_eq!(matched.mock.response, json!({"id": 1, "name": "Alice"}));
        assert_eq!(
            matched.path_params.get("id").map(String::as_str),
            Some("42")
        );
    }

    /// The importer must not register two indistinguishable mocks for one
    /// route. `/users/{id}` documents both a 200 and a 404; if both were
    /// live they would tie on score and the served response would come down
    /// to directory read order, which varies by filesystem.
    #[test]
    fn test_only_the_primary_status_is_live_per_route() {
        let dir = tempfile::tempdir().unwrap();
        write_mocks(&generate(&fixture()), dir.path(), false, OutputFormat::Json).unwrap();

        let loaded = crate::loader::load_mocks_map(dir.path().to_str().unwrap());
        for (key, mocks) in &loaded.mocks {
            assert_eq!(
                mocks.len(),
                1,
                "route {} has {} live mocks, so which one answers is read-order dependent",
                key,
                mocks.len()
            );
        }

        // The alternative response is on disk, just inert until renamed.
        assert!(dir.path().join("get_users_id_404.json.disabled").exists());
        assert!(!dir.path().join("get_users_id_404.json").exists());
    }

    /// Dropping the `.disabled` suffix is all it takes to enable one.
    #[test]
    fn test_renaming_a_disabled_file_enables_it() {
        let dir = tempfile::tempdir().unwrap();
        write_mocks(&generate(&fixture()), dir.path(), false, OutputFormat::Json).unwrap();

        fs::remove_file(dir.path().join("get_users_id.json")).unwrap();
        fs::rename(
            dir.path().join("get_users_id_404.json.disabled"),
            dir.path().join("get_users_id_404.json"),
        )
        .unwrap();

        let loaded = crate::loader::load_mocks_map(dir.path().to_str().unwrap());
        let context =
            crate::matcher::RequestContext::new("GET".to_string(), "/users/42".to_string());
        let matched = crate::matcher::find_matching_mock(&context, &loaded.mocks, None).unwrap();

        assert_eq!(matched.mock.status, 404);
        assert_eq!(matched.mock.response, json!({"error": "not found"}));
    }

    #[test]
    fn test_split_extension() {
        assert_eq!(split_extension("get_a.json"), ("get_a", ".json"));
        assert_eq!(
            split_extension("get_a_404.json.disabled"),
            ("get_a_404", ".json.disabled")
        );
        assert_eq!(split_extension("get_a.yaml"), ("get_a", ".yaml"));
        assert_eq!(
            split_extension("get_a_404.yaml.disabled"),
            ("get_a_404", ".yaml.disabled")
        );
    }

    #[test]
    fn test_generated_brace_style_mocks_also_match() {
        let dir = tempfile::tempdir().unwrap();
        let mocks = generate_mocks(
            &fixture(),
            DEFAULT_STATUS,
            PathStyle::Brace,
            OutputFormat::Json,
            "",
        );
        write_mocks(&mocks, dir.path(), false, OutputFormat::Json).unwrap();

        let loaded = crate::loader::load_mocks_map(dir.path().to_str().unwrap());
        let context =
            crate::matcher::RequestContext::new("GET".to_string(), "/users/7".to_string());

        let matched = crate::matcher::find_matching_mock(&context, &loaded.mocks, None)
            .expect("brace-style mock should match too");
        assert_eq!(matched.mock.path, "/users/{id}");
    }
}
