//! `mimic validate` — a check of a mocks directory against everything the
//! running server would refuse, warn about, or quietly fail to serve.
//!
//! ```text
//! mimic validate [<dir>] [--strict] [--format text|json]
//! ```
//!
//! The directory is loaded through [`load_mocks_map`] — the exact entry the
//! server uses at startup — so a mock the loader skips is a mock this
//! reports, and a mock this accepts is one the server registers. On top of
//! the loader's per-file failures it runs the checks that are only worth
//! doing when nobody is waiting on a response: duplicate routes across
//! files, matchers whose regex cannot compile, sequence steps a
//! `repeat: true` pins into permanent silence.
//!
//! Everything is classified:
//!
//! - an **error** means the mock cannot do what it says (an invalid status,
//!   both `response` and `response_file`, a `response_file` that is missing,
//!   oversized or outside the mocks root, a file that does not parse, a path
//!   that cannot be requested, a route Mimic reserves for itself);
//! - a **warning** means it is suspicious but legal (a duplicate route whose
//!   tie the file name breaks, an uncompilable matcher regex, an unreachable
//!   sequence step, a `{{…}}` expression Mimic renders as an empty string,
//!   an activated tag no mock declares).
//!
//! Errors exit `1`; warnings exit `0` unless `--strict` was given, which is
//! the mode a CI job wants. Bad usage exits `2` with the usage text; a clean
//! directory exits `0` after a one-line summary.
//!
//! Nothing is served, bound, or written: the subcommand runs before the
//! async runtime spins up, makes its report, and exits.

use crate::handler::{
    configured_active_tags, content_type_for_file, is_textual_content_type, ReservedRoutes,
    ACTIVE_TAGS_ENV,
};
use crate::loader::{load_mocks_map, resolve_mocks_dir, LoadError, LoadResult, MOCKS_DIR_ENV};
use crate::regex_cache::compiled_regex;
use crate::template::{is_known_source, template_regex, KNOWN_CASTS};
use crate::types::{
    create_mock_key, is_valid_status, BodyMatcher, HeaderPattern, HeaderValue, MockConfig,
    QueryParamPattern, QueryParamValue,
};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

/// Usage text, printed for `--help` (stdout, exit 0) and for bad usage
/// (stderr, exit 2), mirroring `mimic import-openapi`'s contract.
pub const USAGE: &str = "\
Usage: mimic validate [<dir>] [options]

Check a mocks directory without starting a server. Loads it exactly as
`mimic serve` would, then reports every problem it found: an *error* is a
mock that cannot do what it says, a *warning* is something suspicious the
server would never mention unless asked.

Options:
  <dir>                Directory — or a single mock file — to validate.
                       Defaults to what the server would read: MIMIC_MOCKS_DIR,
                       then /app/mocks, then ./mocks.
  --strict             Exit non-zero on warnings too (what a CI job wants).
  --format <fmt>       Report format: text or json (default: text).
  -h, --help           Show this help.

Exit status:
  0                    No problems (no errors, and with --strict no warnings).
  1                    At least one error, or warnings under --strict.
  2                    Bad usage.
";

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug)]
pub enum ValidateError {
    /// Bad command line; an empty message means `--help`, which is not an
    /// error — the caller prints `USAGE` to stdout and exits 0.
    Usage(String),
}

impl fmt::Display for ValidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidateError::Usage(msg) => write!(f, "{}", msg),
        }
    }
}

// ============================================================================
// Options
// ============================================================================

/// Report format for the run, chosen with `--format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ValidateOptions {
    /// Explicit `<dir>` argument. `None` means "what the server would read":
    /// resolution runs through `resolve_mocks_dir`, so a validate and a serve
    /// cannot disagree about what is on disk.
    pub dir: Option<PathBuf>,
    /// `--strict`: warnings must not exist either.
    pub strict: bool,
    /// `--format`; text unless asked otherwise.
    pub format: Format,
}

/// Parse the arguments that follow `validate`.
pub fn parse_args(args: &[String]) -> Result<ValidateOptions, ValidateError> {
    let mut options = ValidateOptions::default();
    let mut dir: Option<PathBuf> = None;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "-h" | "--help" => return Err(ValidateError::Usage(String::new())),
            "--strict" => options.strict = true,
            "--format" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| ValidateError::Usage("--format requires a value".to_string()))?;
                match value.as_str() {
                    "text" => options.format = Format::Text,
                    "json" => options.format = Format::Json,
                    other => {
                        return Err(ValidateError::Usage(format!(
                            "unknown --format value '{}': expected 'text' or 'json'",
                            other
                        )))
                    }
                }
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(ValidateError::Usage(format!("unknown option '{}'", other)));
            }
            other => {
                if dir.is_some() {
                    return Err(ValidateError::Usage(format!(
                        "unexpected extra argument '{}'",
                        other
                    )));
                }
                dir = Some(PathBuf::from(other));
            }
        }
        index += 1;
    }

    options.dir = dir;
    Ok(options)
}

// ============================================================================
// Findings and the report
// ============================================================================

/// How bad a finding is: an error fails the run; a warning explains itself
/// and fails a `--strict` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One problem in one file, plus a stable kind code for the JSON shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The mock file the problem is in, as the walk named it (a leading
    /// `./` is stripped, so the column is a path someone can click). For a
    /// problem the whole mock set shares, this is the directory itself.
    pub file: String,
    pub severity: Severity,
    /// Stable snake_case code for `--format json` consumers, e.g.
    /// `invalid_status`.
    pub kind: &'static str,
    pub message: String,
}

/// The whole outcome of one `mimic validate` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The directory (or single file) that was validated, as given or
    /// resolved — a leading `./` is stripped.
    pub directory: String,
    /// How many mock files the loader scanned — the denominator the summary
    /// line talks about.
    pub files: usize,
    /// Every finding, ordered by (file, severity, kind, message).
    pub problems: Vec<Finding>,
}

impl Report {
    pub fn errors(&self) -> usize {
        self.problems
            .iter()
            .filter(|problem| problem.severity == Severity::Error)
            .count()
    }

    pub fn warnings(&self) -> usize {
        self.problems
            .iter()
            .filter(|problem| problem.severity == Severity::Warning)
            .count()
    }
}

// ============================================================================
// The checks
// ============================================================================

/// Every finding about a [`LoadResult`].
///
/// `active_tags` is the scenario selection the server would start with —
/// `None` when `MIMIC_ACTIVE_TAGS` is unset, which leaves scenarios free to
/// be activated later, so nothing can be called dormant by mistake. The other
/// arguments come from the same sources the server itself consults at
/// startup: `ReservedRoutes::from_env` and the loader. That is the whole
/// point — there is no second implementation to drift from the server's.
fn collect_findings(
    result: &LoadResult,
    reserved: &ReservedRoutes,
    active_tags: Option<&HashSet<String>>,
    directory: &str,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // What the loader itself refused to load: an unparsable file, both
    // `response` and `response_file`, a fixture that is missing, oversized or
    // outside the mocks root. These are errors — each one is a mock the
    // server never registers at all. An element of an array file that failed
    // validation is reported as `file#index`; its siblings that did load are
    // checked like every other mock below.
    for error in &result.errors {
        findings.push(Finding {
            file: display_path(Path::new(&error.target())),
            severity: Severity::Error,
            kind: "load_error",
            message: message_for_report(error),
        });
    }

    // Everything else checks mocks that DID load — registered by the server
    // and then either served differently than declared or never served.
    for mock in result.mocks.values().flatten() {
        let file = source_of(mock);
        check_reserved_route(mock, &file, reserved, &mut findings);
        check_status(mock, &file, &mut findings);
        check_path(mock, &file, &mut findings);
        check_regexes(mock, &file, &mut findings);
        check_sequence(mock, &file, &mut findings);
        check_templates(mock, &file, &mut findings);
    }

    check_duplicates(result, &mut findings);
    check_active_tags(result, active_tags, directory, &mut findings);
    sort_findings(&mut findings);
    findings
}

/// The file a finding attributes a mock to, with the `#index` of an array
/// element when it has one — always set by the loader; the fallback is what
/// the server's own diagnostics would say for a mock that never came from a
/// file.
fn source_of(mock: &MockConfig) -> String {
    let walked = mock
        .source_label()
        .unwrap_or_else(|| "(built in-process)".to_string());
    display_path(Path::new(&walked))
}

/// Route Mimic answers itself: loaded, listed on `/admin/mocks`, never
/// served. The same rule the server applies at startup (`shadowed_mocks`).
fn check_reserved_route(
    mock: &MockConfig,
    file: &str,
    reserved: &ReservedRoutes,
    findings: &mut Vec<Finding>,
) {
    if let Some(owner) = reserved.reservation_for(&mock.method, &mock.path) {
        findings.push(Finding {
            file: file.to_string(),
            severity: Severity::Error,
            kind: "reserved_route",
            message: format!(
                "{} {} is reserved by {} and will never be served. Move it, or free the \
                 route (see MIMIC_HEALTH_PATH / MIMIC_ADMIN_PREFIX / MIMIC_DISABLE_ADMIN).",
                mock.method, mock.path, owner
            ),
        });
    }
}

/// A status the server cannot put on the wire — on the mock, and on every
/// sequence step, since a step is the response when its turn comes.
fn check_status(mock: &MockConfig, file: &str, findings: &mut Vec<Finding>) {
    if !is_valid_status(mock.status) {
        findings.push(Finding {
            file: file.to_string(),
            severity: Severity::Error,
            kind: "invalid_status",
            message: format!(
                "status {} is not a valid HTTP status code (100-599); requests would \
                 be answered 200 OK instead",
                mock.status
            ),
        });
    }

    for (index, step) in mock.sequence.iter().flatten().enumerate() {
        if !is_valid_status(step.status) {
            findings.push(Finding {
                file: file.to_string(),
                severity: Severity::Error,
                kind: "invalid_status",
                message: format!(
                    "sequence step {} declares status {}, which is not a valid HTTP \
                     status code (100-599); it would be served as 200 OK",
                    index + 1,
                    step.status
                ),
            });
        }
    }
}

/// A `path` no request can ever name: requests arrive as `/…`, keys are
/// built as `METHOD:/path`, and `"users"` registers as `GET:users` — a key
/// nothing ever looks up.
fn check_path(mock: &MockConfig, file: &str, findings: &mut Vec<Finding>) {
    if !mock.path.starts_with('/') {
        findings.push(Finding {
            file: file.to_string(),
            severity: Severity::Error,
            kind: "path_without_slash",
            message: format!(
                "path '{}' does not start with '/': it registers under '{}', while \
                 requests look up '{}:/…', so it can never be served",
                mock.path,
                create_mock_key(&mock.method, &mock.path),
                mock.method.to_uppercase()
            ),
        });
    }
}

/// Regexes declared in matchers, through the same `compiled_regex` the
/// matcher consults at request time — a pattern that fails here answers "no"
/// to every request, silently.
fn check_regexes(mock: &MockConfig, file: &str, findings: &mut Vec<Finding>) {
    if let Some(query) = &mock.query_params {
        for (name, value) in &query.params {
            if let QueryParamValue::Pattern(QueryParamPattern::Regex(regex)) = value {
                if compiled_regex(regex).is_none() {
                    findings.push(Finding {
                        file: file.to_string(),
                        severity: Severity::Warning,
                        kind: "invalid_regex",
                        message: format!(
                            "query param `{}` regex `{}` failed to compile; this \
                             matcher can never match",
                            name, regex
                        ),
                    });
                }
            }
        }
    }

    if let Some(headers) = &mock.headers {
        for (name, value) in &headers.required {
            if let HeaderValue::Pattern(HeaderPattern::Regex(regex)) = value {
                if compiled_regex(regex).is_none() {
                    findings.push(Finding {
                        file: file.to_string(),
                        severity: Severity::Warning,
                        kind: "invalid_regex",
                        message: format!(
                            "header `{}` regex `{}` failed to compile; this matcher \
                             can never match",
                            name, regex
                        ),
                    });
                }
            }
        }
    }

    if let Some(BodyMatcher::Text(text)) = &mock.body {
        if let Some(regex) = &text.regex {
            if compiled_regex(regex).is_none() {
                findings.push(Finding {
                    file: file.to_string(),
                    severity: Severity::Warning,
                    kind: "invalid_regex",
                    message: format!(
                        "text body matcher regex `{}` failed to compile; this matcher \
                         can never match",
                        regex
                    ),
                });
            }
        }
    }
}

/// The sequence rule `advance_sequence` enforces when a request actually
/// arrives: a step with `repeat: true` stops the counter from advancing, so
/// every step after it is dead weight. Legal — but a sequence that ends
/// there is almost never what the author meant.
fn check_sequence(mock: &MockConfig, file: &str, findings: &mut Vec<Finding>) {
    let Some(steps) = mock.sequence.as_deref() else {
        return;
    };
    let Some(pins) = steps.iter().position(|step| step.repeat) else {
        return;
    };
    let unreachable = steps.len() - pins - 1;
    if unreachable > 0 {
        findings.push(Finding {
            file: file.to_string(),
            severity: Severity::Warning,
            kind: "unreachable_sequence_step",
            message: format!(
                "{} step(s) can never serve: step {} declares `repeat: true`, which \
                 pins the counter at it until the sequence is reset",
                unreachable,
                pins + 1
            ),
        });
    }
}

/// A `{{…}}` expression is only a lie if the renderer agrees with the mock
/// about what happens, and the renderer's own gates decide which bodies get
/// interpolated at all: a `response` is always scanned for expressions; a
/// file-backed body is scanned only when the mock opted in with
/// `"template": true` and the file's content type is textual. Anything else
/// is served verbatim — so a validator that scanned it there would be
/// calling harmless content a problem.
fn check_templates(mock: &MockConfig, file: &str, findings: &mut Vec<Finding>) {
    let mut problems = Vec::new();

    scan_value_for_template_problems(&mock.response, "the top-level `response`", &mut problems);

    if let Some(response_file) = &mock.response_file {
        scan_file_body_problems(
            response_file,
            mock.response_bytes.as_deref(),
            mock.template.unwrap_or(false),
            &mut problems,
        );
    }

    for (index, step) in mock.sequence.iter().flatten().enumerate() {
        scan_value_for_template_problems(
            &step.response,
            &format!("the `response` of sequence step {}", index + 1),
            &mut problems,
        );
        if let Some(response_file) = &step.response_file {
            scan_file_body_problems(
                response_file,
                step.response_bytes.as_deref(),
                step.template.unwrap_or(false),
                &mut problems,
            );
        }
    }

    for TemplateProblem {
        expression,
        context,
        reason,
    } in problems
    {
        findings.push(Finding {
            file: file.to_string(),
            severity: Severity::Warning,
            kind: "unknown_template_source",
            message: format!("`{}` in {} {}", as_expression(&expression), context, reason),
        });
    }
}

/// One expression that will not do what the mock says it will.
struct TemplateProblem {
    /// The expression as the mock file spells it, e.g. `{{cookie.x}}`.
    expression: String,
    /// Where in the mock file it was found, e.g. "the top-level `response`".
    context: String,
    /// Why the expression is a lie, as an ongoing sentence.
    reason: String,
}

/// The expression as the mock file spells it — `{{…}}`, built by hand so no
/// brace-escaping has to survive this file's format strings.
fn as_expression(raw: &str) -> String {
    let mut shown = String::with_capacity(raw.len() + 4);
    shown.push_str("{{");
    shown.push_str(raw);
    shown.push_str("}}");
    shown
}

fn scan_value_for_template_problems(
    value: &Value,
    context: &str,
    problems: &mut Vec<TemplateProblem>,
) {
    match value {
        Value::String(text) => scan_text_for_template_problems(text, context, problems),
        Value::Object(map) => {
            for value in map.values() {
                scan_value_for_template_problems(value, context, problems);
            }
        }
        Value::Array(items) => {
            for item in items {
                scan_value_for_template_problems(item, context, problems);
            }
        }
        _ => {}
    }
}

/// The same gate `build_response_parts` applies: only an opted-in, textual,
/// UTF-8 file body is rendered at all — and this infers the content type the
/// way the server does, from the extension.
fn scan_file_body_problems(
    response_file: &str,
    bytes: Option<&[u8]>,
    template: bool,
    problems: &mut Vec<TemplateProblem>,
) {
    let content_type = content_type_for_file(response_file);
    if !template || !is_textual_content_type(content_type) {
        return;
    }
    let Some(bytes) = bytes else {
        return;
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    scan_text_for_template_problems(
        text,
        &format!("`response_file` '{}'", response_file),
        problems,
    );
}

fn scan_text_for_template_problems(text: &str, context: &str, problems: &mut Vec<TemplateProblem>) {
    if !text.contains("{{") {
        return;
    }
    for caps in template_regex().captures_iter(text) {
        let expression = caps.get(0).unwrap().as_str().to_string();

        if let Some(cast) = caps.get(1).map(|cast| cast.as_str()) {
            if !KNOWN_CASTS.contains(&cast) {
                // An unknown cast word downgrades the *whole* expression to
                // an empty string, source and all — see `interpolate`.
                problems.push(TemplateProblem {
                    expression,
                    context: context.to_string(),
                    reason: format!(
                        "casts through `{}`, which Mimic does not know (casts: {}); \
                         it renders as an empty string",
                        cast,
                        KNOWN_CASTS.join(", ")
                    ),
                });
                continue;
            }
        }

        if !is_known_source(&caps[2]) {
            problems.push(TemplateProblem {
                expression,
                context: context.to_string(),
                reason: format!(
                    "names source `{}`, which Mimic does not resolve; it renders as \
                     an empty string",
                    &caps[2]
                ),
            });
        }
    }
}

/// Several files claiming one `METHOD:path` are legal — `find_matching_mock`
/// breaks the tie. But the tie is broken by file name (the winner is
/// whichever sorted first), so the loser deserves a line saying so, not a
/// dashboard row with `hits: 0` and no explanation.
fn check_duplicates(result: &LoadResult, findings: &mut Vec<Finding>) {
    let mut buckets: Vec<(&String, &Vec<MockConfig>)> = result.mocks.iter().collect();
    buckets.sort_by(|a, b| a.0.cmp(b.0));

    for (key, bucket) in buckets {
        if bucket.len() < 2 {
            continue;
        }
        let winner = source_of(&bucket[0]);
        let (method, path) = match key.split_once(':') {
            Some((method, path)) => (method, path),
            None => continue,
        };
        for mock in &bucket[1..] {
            findings.push(Finding {
                file: source_of(mock),
                severity: Severity::Warning,
                kind: "duplicate_route",
                message: format!(
                    "duplicate {} {} (also: {}) — ties are broken by file name, so \
                     this copy only serves when the first one is inactive",
                    method, path, winner
                ),
            });
        }
    }
}

/// Tags the current configuration activates but no mock declares. The
/// reverse — a declared tag that is not active right now — is deliberately
/// not reported: scenarios are switched at runtime (`MIMIC_ACTIVE_TAGS`,
/// `POST /admin/scenario`), so "inactive" is a state, not a bug, and a
/// validator that fails `--strict` over a deliberately dormant scenario is
/// worse than no check.
fn check_active_tags(
    result: &LoadResult,
    active_tags: Option<&HashSet<String>>,
    directory: &str,
    findings: &mut Vec<Finding>,
) {
    let Some(active_tags) = active_tags else {
        return;
    };

    let declared: HashSet<&str> = result
        .mocks
        .values()
        .flatten()
        .flat_map(|mock| mock.tags.iter().map(String::as_str))
        .collect();

    let mut unknown: Vec<&String> = active_tags
        .iter()
        .filter(|tag| !declared.contains(tag.as_str()))
        .collect();
    unknown.sort();
    for tag in unknown {
        findings.push(Finding {
            file: directory.to_string(),
            severity: Severity::Warning,
            kind: "unknown_active_tag",
            message: format!(
                "{} activates tag '{}', which no mock declares; the scenario serves \
                 nothing",
                ACTIVE_TAGS_ENV, tag
            ),
        });
    }
}

/// Findings in a deterministic order — the report is a product (and its JSON
/// is diffed), not a HashMap's opinion of the day.
fn sort_findings(findings: &mut Vec<Finding>) {
    findings.sort_by(|a, b| {
        (&a.file, a.severity, a.kind, &a.message).cmp(&(&b.file, b.severity, b.kind, &b.message))
    });
    findings.dedup();
}

/// The path a finding's `file` column shows: the walked path, minus the
/// leading `./` a relative call adds.
fn display_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    raw.strip_prefix("./")
        .map(str::to_owned)
        .unwrap_or_else(|| raw.to_string())
}

/// Loader error messages mostly embed the target they are about — the file,
/// or `file#index` for one element of an array file; the finding's file
/// column would repeat it. Strip the shapes the loader actually emits —
/// nothing else is rewritten.
fn message_for_report(error: &LoadError) -> String {
    // The target as the loader embedded it — with the `./` the report column
    // strips, if it had one.
    let embedded = error.target();
    let message = &error.message;

    // Two messages carry no payload beyond the rule; rewrite them as a sentence.
    if message == &format!("Empty method in {}", embedded) {
        return "mock declares an empty `method`".to_string();
    }
    if message == &format!("Empty path in {}", embedded) {
        return "mock declares an empty `path`".to_string();
    }

    let stripped = [
        format!("Failed to parse JSON in {}: ", embedded),
        format!("Failed to parse YAML in {}: ", embedded),
        format!("Failed to read file {}: ", embedded),
        format!("{} sets both ", embedded),
        format!("{}: ", embedded),
    ]
    .into_iter()
    .find_map(|prefix| message.strip_prefix(prefix.as_str()))
    .map(str::to_owned)
    .unwrap_or_else(|| message.to_string());

    stripped
}

// ============================================================================
// Rendering
// ============================================================================

/// The report as text — one row per problem, columns aligned, then the
/// summary line. The summary is always printed, clean or not, because it is
/// the one machine-readable thing a shell script can grep without parsing.
fn render_text(report: &Report, strict: bool) -> String {
    let width = report
        .problems
        .iter()
        .map(|problem| problem.file.len() + 1)
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for problem in &report.problems {
        out.push_str(&format!(
            "{:<width$}  {:<7} {}\n",
            format!("{}:", problem.file),
            problem.severity.as_str(),
            problem.message
        ));
    }

    let mut summary = format!(
        "{} problem(s) in {} mock file(s): {} error(s), {} warning(s)",
        report.problems.len(),
        report.files,
        report.errors(),
        report.warnings()
    );
    if strict && report.warnings() > 0 {
        summary.push_str(" (--strict: warnings counted as errors)");
    }
    out.push_str(&summary);
    out
}

/// The report as JSON — a stable shape for whatever wants to annotate a PR:
/// the same fields every time, in the same order, and `kind` codes that do
/// not change meaning between releases.
fn render_json(report: &Report, strict: bool) -> String {
    let problems: Vec<Value> = report
        .problems
        .iter()
        .map(|problem| {
            serde_json::json!({
                "file": problem.file,
                "severity": problem.severity.as_str(),
                "kind": problem.kind,
                "message": problem.message,
            })
        })
        .collect();

    let output = serde_json::json!({
        "directory": report.directory,
        "files": report.files,
        "strict": strict,
        "errors": report.errors(),
        "warnings": report.warnings(),
        "problems": problems,
    });

    serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
}

/// `0` clean; `1` when any error — or, under `--strict`, any warning; `2`
/// never comes from here, it is the bad-usage exit the arg parser signals.
fn exit_code(report: &Report, strict: bool) -> i32 {
    if report.errors() > 0 || (strict && report.warnings() > 0) {
        1
    } else {
        0
    }
}

// ============================================================================
// The subcommand
// ============================================================================

/// Run `mimic validate`, returning the process exit code.
pub fn run_validate(args: &[String]) -> i32 {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(ValidateError::Usage(message)) => {
            // An empty message means `--help`, which isn't an error.
            if message.is_empty() {
                print!("{}", USAGE);
                return 0;
            }
            eprintln!("error: {}", message);
            eprint!("\n{}", USAGE);
            return 2;
        }
    };

    // The explicit argument wins; with none, the same resolution the server
    // performs decides — `MIMIC_MOCKS_DIR`, then the Docker mount, then
    // `./mocks`.
    let resolved = resolve_dir(options.dir.as_deref());
    if !Path::new(&resolved).exists() {
        eprintln!(
            "error: mocks path '{}' does not exist; pass a directory, or set {}=<dir>",
            resolved, MOCKS_DIR_ENV
        );
        return 1;
    }

    let directory = display_path(Path::new(&resolved));
    let result = load_mocks_map(&resolved);
    let reserved = ReservedRoutes::from_env();
    let active_tags = configured_active_tags();
    let findings = collect_findings(&result, &reserved, active_tags.as_ref(), &directory);

    let report = Report {
        directory,
        files: result.files,
        problems: findings,
    };

    let rendered = match options.format {
        Format::Text => render_text(&report, options.strict),
        Format::Json => render_json(&report, options.strict),
    };
    println!("{}", rendered);
    exit_code(&report, options.strict)
}

/// The directory (or single file) this run validates. An explicit argument is
/// used verbatim; otherwise the resolution the server performs decides, so a
/// validate and a serve cannot disagree about which mocks are loaded.
fn resolve_dir(explicit: Option<&Path>) -> String {
    match explicit {
        Some(dir) => dir.display().to_string(),
        None => resolve_mocks_dir().path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::{load_mocks_map_with_limit, DEFAULT_MAX_RESPONSE_FILE};
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    /// The reservations a default `mimic serve` would enforce.
    fn default_reserved() -> ReservedRoutes {
        ReservedRoutes::from_values(None, None, None)
    }

    /// Every finding about a directory of mocks, with the defaults and the
    /// scenario selection injected — tests never consult process env, and
    /// the findings come through the real loader.
    fn findings(dir: &TempDir, active_tags: Option<&HashSet<String>>) -> Vec<Finding> {
        let result =
            load_mocks_map_with_limit(dir.path().to_str().unwrap(), DEFAULT_MAX_RESPONSE_FILE);
        collect_findings(&result, &default_reserved(), active_tags, "mocks")
    }

    fn one(dir: &TempDir) -> Finding {
        let findings = findings(dir, None);
        assert_eq!(
            findings.len(),
            1,
            "expected exactly one finding, got: {:?}",
            findings
        );
        findings.into_iter().next().unwrap()
    }

    /// Write mock files into a fresh directory; the map is (name, contents).
    fn mocks_dir(files: &[(&str, &[u8])]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, contents) in files {
            fs::write(dir.path().join(name), contents).unwrap();
        }
        dir
    }

    fn tags<const N: usize>(values: [&str; N]) -> HashSet<String> {
        values.iter().map(|tag| tag.to_string()).collect()
    }

    // ------------------------------------------------------------------
    // The clean case
    // ------------------------------------------------------------------

    #[test]
    fn a_clean_directory_reports_zero_problems() {
        let dir = mocks_dir(&[
            (
                "users.json",
                br#"{"method":"GET","path":"/users","status":200,"response":{"users":[]}}"#,
            ),
            (
                "login.json",
                br#"{"method":"POST","path":"/login","status":201,"response":{"token":"x"}}"#,
            ),
        ]);

        let found = findings(&dir, None);
        assert!(found.is_empty(), "clean set must be silent: {:?}", found);
    }

    #[test]
    fn every_mock_file_counts_toward_files() {
        let dir = mocks_dir(&[
            (
                "users.json",
                br#"{"method":"GET","path":"/users","status":200,"response":{}}"#,
            ),
            ("broken.json", b"{"),
        ]);

        let result =
            load_mocks_map_with_limit(dir.path().to_str().unwrap(), DEFAULT_MAX_RESPONSE_FILE);
        assert_eq!(
            result.files, 2,
            "one parsed, one refused — both were scanned"
        );
    }

    // ------------------------------------------------------------------
    // Errors: the mock can't do what it says
    // ------------------------------------------------------------------

    #[test]
    fn an_unparsable_file_is_an_error_naming_the_rule_without_repeating_the_path() {
        let dir = mocks_dir(&[("broken.json", b"{ not json")]);

        let finding = one(&dir);
        assert_eq!(finding.kind, "load_error");
        assert_eq!(finding.severity, Severity::Error);
        assert!(
            finding.message.contains("key must be a string"),
            "the parser's own diagnosis survives the trim: {}",
            finding.message
        );
        assert!(
            !finding.message.contains("Failed to parse"),
            "the file is already its own column; no room for a header: {}",
            finding.message
        );
    }

    #[test]
    fn both_bodies_refuse_to_load_and_are_reported() {
        let dir = mocks_dir(&[(
            "both.json",
            br#"{"method":"GET","path":"/both","status":200,
                 "response":{"a":1},"response_file":"body.txt"}"#,
        )]);

        let finding = one(&dir);
        assert_eq!(finding.kind, "load_error");
        assert!(
            finding.message.contains("one or the other"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn a_missing_response_file_refuses_to_load() {
        let dir = mocks_dir(&[(
            "gone.json",
            br#"{"method":"GET","path":"/gone","status":200,"response_file":"missing.txt"}"#,
        )]);

        let finding = one(&dir);
        assert_eq!(finding.kind, "load_error");
        assert!(
            finding.message.contains("cannot read response_file"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn a_response_file_outside_the_mocks_root_refuses_to_load() {
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret"), b"password").unwrap();
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("leak.json"),
            format!(
                r#"{{"method":"GET","path":"/leak","status":200,
                     "response_file":"../{}/secret"}}"#,
                outside.path().file_name().unwrap().to_str().unwrap()
            ),
        )
        .unwrap();

        let finding = one(&dir);
        assert_eq!(finding.kind, "load_error");
        assert!(
            finding.message.contains("outside the mocks root"),
            "the rule names itself: {}",
            finding.message
        );
    }

    #[test]
    fn an_oversized_response_file_refuses_to_load_at_the_injected_limit() {
        let dir = mocks_dir(&[
            (
                "big.json",
                br#"{"method":"GET","path":"/big","status":200,"response_file":"big.bin"}"#,
            ),
            ("big.bin", b"way more than one byte"),
        ]);
        // The cap is injected exactly for this: `--format json` runs must
        // agree with whatever `MIMIC_MAX_RESPONSE_FILE` the server was given.
        let result = load_mocks_map_with_limit(dir.path().to_str().unwrap(), 1);

        let mut findings = collect_findings(&result, &default_reserved(), None, "mocks");
        assert_eq!(findings.len(), 1, "{:?}", findings);
        let finding = findings.remove(0);
        assert_eq!(finding.kind, "load_error");
        assert!(
            finding.message.contains("over the 1-byte"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn an_invalid_status_is_an_error_for_the_mock_and_for_every_sequence_step() {
        let dir = mocks_dir(&[(
            "checkout.json",
            br#"{"method":"POST","path":"/checkout","status":1000,"response":{},
                 "sequence":[
                    {"status":700,"response":{"a":1}},
                    {"status":200,"response":{"b":2}}
                 ]}"#,
        )]);

        let found = findings(&dir, None);
        assert_eq!(found.len(), 2, "{:?}", found);
        assert!(found.iter().all(|finding| finding.kind == "invalid_status"));
        assert!(found
            .iter()
            .all(|finding| finding.severity == Severity::Error));
        let (first, second) = (&found[0], &found[1]);
        let (first, second) = if first.message.contains("sequence step 1") {
            (first, second)
        } else {
            (second, first)
        };
        assert!(
            first.message.contains("status 700") && first.message.contains("sequence step 1"),
            "the message names 100-599 and the step: {}",
            first.message
        );
        assert!(
            second.message.contains("status 1000"),
            "the mock's own status is checked too: {}",
            second.message
        );
    }

    // ------------------------------------------------------------------
    // Array files (#142)
    // ------------------------------------------------------------------

    #[test]
    fn an_element_load_error_is_reported_on_file_index() {
        let dir = mocks_dir(&[(
            "orders.json",
            br#"[
                {"method":"GET","path":"/orders","status":200,"response":{"ok":true}},
                {"method":"","path":"/orders","status":201,"response":{"ok":true}}
            ]"#,
        )]);

        let finding = one(&dir);
        assert_eq!(finding.kind, "load_error");
        assert!(
            finding.file.ends_with("orders.json#1"),
            "the column names the element, not just the file: {:?}",
            finding.file
        );
        assert!(
            finding.message.contains("empty `method`"),
            "the rule survives the trim: {:?}",
            finding.message
        );
    }

    #[test]
    fn mock_checks_on_an_array_element_name_the_element() {
        // A mock that DID load is checked like any other; its findings point
        // at the element it is, so a report can be acted on file-by-file.
        let dir = mocks_dir(&[(
            "orders.json",
            br#"[
                {"method":"GET","path":"/orders","status":200,"response":{"ok":true}},
                {"method":"GET","path":"badpath","status":1000,"response":{"ok":true}},
                {"method":"GET","path":"/orders2","status":200,"response":{}}
            ]"#,
        )]);

        let found = findings(&dir, None);
        assert_eq!(found.len(), 2, "{:?}", found);
        assert!(found.iter().all(|f| f.file.ends_with("orders.json#1")));
        assert!(found.iter().all(|f| f.severity == Severity::Error));
        assert!(found.iter().any(|f| f.kind == "invalid_status"));
        assert!(found.iter().any(|f| f.kind == "path_without_slash"));
    }

    #[test]
    fn a_whole_file_array_failure_is_reported_on_the_file() {
        let dir = mocks_dir(&[
            (
                "users.json",
                br#"{"method":"GET","path":"/users","status":200,"response":{}}"#,
            ),
            ("broken.json", b"[{\"method\":\"GET\""),
        ]);

        let found = findings(&dir, None);
        assert_eq!(found.len(), 1, "{:?}", found);
        assert!(
            found[0].file.ends_with("broken.json"),
            "{:?}",
            found[0].file
        );
        assert_eq!(found[0].kind, "load_error");
        assert!(
            found[0].message.contains("EOF while parsing"),
            "the parser's own diagnosis survives the trim: {:?}",
            found[0].message
        );
    }

    #[test]
    fn a_duplicate_route_within_one_array_file_warns_per_element() {
        let dir = mocks_dir(&[(
            "orders.json",
            br#"[
                {"method":"GET","path":"/orders","status":200,"response":{"which":1}},
                {"method":"GET","path":"/orders","status":200,"response":{"which":2}}
            ]"#,
        )]);

        let found = findings(&dir, None);
        assert_eq!(found.len(), 1, "{:?}", found);
        assert_eq!(found[0].kind, "duplicate_route");
        assert_eq!(found[0].severity, Severity::Warning);
        assert!(
            found[0].file.ends_with("orders.json#1"),
            "the loser names its element: {:?}",
            found[0].file
        );
        assert!(
            found[0].message.contains("orders.json#0"),
            "the winner names its element too: {:?}",
            found[0].message
        );
    }

    #[test]
    fn reserved_routes_refuse_the_route_for_the_method_they_are_reserved_with() {
        let dir = mocks_dir(&[
            (
                "health.json",
                br#"{"method":"GET","path":"/health","status":200,"response":{"fake":true}}"#,
            ),
            (
                "dashboard.json",
                br#"{"method":"GET","path":"/admin/mocks","status":200,"response":{}}"#,
            ),
            (
                // `/admin/mocks` is reserved for GET; POST falls through to
                // the mock fallback and is a perfectly good mock.
                "post_admin.json",
                br#"{"method":"POST","path":"/admin/mocks","status":201,"response":{}}"#,
            ),
        ]);

        let found = findings(&dir, None);
        assert_eq!(found.len(), 2, "{:?}", found);
        assert!(found.iter().all(|finding| finding.kind == "reserved_route"));
        assert!(found
            .iter()
            .all(|finding| finding.message.contains("will never be served")));
        assert!(
            found
                .iter()
                .all(|finding| !finding.file.ends_with("post_admin.json")),
            "only the reserved pairs are reported, not the free fallback: {:?}",
            found
        );
    }

    #[test]
    fn a_path_without_the_leading_slash_registers_a_key_no_request_uses() {
        let dir = mocks_dir(&[(
            "users.json",
            br#"{"method":"get","path":"users","status":200,"response":{}}"#,
        )]);

        let finding = one(&dir);
        assert_eq!(finding.kind, "path_without_slash");
        assert_eq!(finding.severity, Severity::Error);
        assert!(
            finding.message.contains("GET:users") && finding.message.contains("GET:/…"),
            "the message shows both keys: {}",
            finding.message
        );
    }

    // ------------------------------------------------------------------
    // Warnings: legal, but only by a rule nobody can see
    // ------------------------------------------------------------------

    #[test]
    fn a_duplicate_route_warns_and_names_the_file_that_wins_the_tie() {
        let dir = mocks_dir(&[
            (
                "a_users.json",
                br#"{"method":"GET","path":"/users","status":200,"response":{"from":"a"}}"#,
            ),
            (
                "b_users.json",
                br#"{"method":"GET","path":"/users","status":200,"response":{"from":"b"}}"#,
            ),
            (
                "unrelated.json",
                br#"{"method":"GET","path":"/other","status":200,"response":{}}"#,
            ),
        ]);

        let found = findings(&dir, None);
        assert_eq!(found.len(), 1, "{:?}", found);
        let finding = found.into_iter().next().unwrap();
        assert_eq!(finding.kind, "duplicate_route");
        assert_eq!(finding.severity, Severity::Warning);
        // The winner is the file that sorted first, and the warning lands on
        // the loser: that's how load-order and match order agree.
        assert!(finding.file.ends_with("b_users.json"), "{}", finding.file);
        assert!(
            finding.message.contains("duplicate GET /users")
                && finding.message.contains("a_users.json"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn uncompilable_matcher_regexes_warn_for_query_headers_and_text_body() {
        let dir = mocks_dir(&[(
            "search.json",
            br#"{"method":"GET","path":"/search","status":200,"response":{},
                 "query_params":{"params":{"q":{"regex":"[a-z"}}},
                 "headers":{"required":{"x-v":{"regex":"("}}},
                 "body":{"type":"text","regex":"a["}}"#,
        )]);

        let found = findings(&dir, None);
        assert_eq!(found.len(), 3, "{:?}", found);
        let rules = found
            .iter()
            .map(|finding| finding.message.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(found.iter().all(|finding| finding.kind == "invalid_regex"));
        assert!(found
            .iter()
            .all(|finding| finding.severity == Severity::Warning));
        assert!(rules.contains("query param `q`"), "{}", rules);
        assert!(rules.contains("header `x-v`"), "{}", rules);
        assert!(rules.contains("text body matcher"), "{}", rules);
    }

    #[test]
    fn steps_after_a_repeat_are_warned_about() {
        let dir = mocks_dir(&[(
            "orders.json",
            br#"{"method":"POST","path":"/orders","status":200,"response":{},
                 "sequence":[
                    {"status":201,"response":{"step":1}},
                    {"status":200,"response":{"step":2},"repeat":true},
                    {"status":500,"response":{"step":3}}
                 ]}"#,
        )]);

        let finding = one(&dir);
        assert_eq!(finding.kind, "unreachable_sequence_step");
        assert_eq!(finding.severity, Severity::Warning);
        assert!(
            finding.message.contains("1 step(s) can never serve")
                && finding.message.contains("step 2 declares `repeat: true`"),
            "1-based step numbers, the pinning step named: {}",
            finding.message
        );
    }

    #[test]
    fn a_repeat_as_the_last_step_is_normal_and_silent() {
        let dir = mocks_dir(&[(
            "orders.json",
            br#"{"method":"POST","path":"/orders","status":200,"response":{},
                 "sequence":[
                    {"status":201,"response":{"step":1}},
                    {"status":200,"response":{"step":2},"repeat":true}
                 ]}"#,
        )]);

        assert!(findings(&dir, None).is_empty());
    }

    #[test]
    fn unknown_template_sources_in_a_response_warn() {
        let dir = mocks_dir(&[(
            "profile.json",
            br#"{"method":"GET","path":"/profile/:id","status":200,
                 "template":true,
                 "response":{"known":"{{path.id}}","unknown":"{{cookie.x}}",
                              "typed":"{{json:body.a}}","cast":"{{nope:body.b}}",
                              "fake":"{{faker.name}}","silent":"{{query.q}} {{header.x}}"}}"#,
        )]);

        let found = findings(&dir, None);
        assert_eq!(found.len(), 2, "{:?}", found);
        let rules = found
            .iter()
            .map(|finding| finding.message.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(found
            .iter()
            .all(|finding| finding.kind == "unknown_template_source"));
        assert!(
            rules.contains("{{cookie.x}}") && rules.contains("source `cookie`"),
            "{}",
            rules
        );
        assert!(
            rules.contains("{{nope:body.b}}") && rules.contains("casts through `nope`"),
            "{}",
            rules
        );
        assert!(
            !rules.contains("{{path.id}}")
                && !rules.contains("{{query.q}}")
                && !rules.contains("{{header.x}}")
                && !rules.contains("{{body.a}}")
                && !rules.contains("{{faker.name}}")
                && !rules.contains("{{json:body.a}}"),
            "known sources and casts are never flagged:\n{}",
            rules
        );
    }

    #[test]
    fn a_templated_textual_file_body_is_checked() {
        let dir = mocks_dir(&[
            (
                "text.json",
                br#"{"method":"GET","path":"/text","status":200,
                     "template":true,"response_file":"reply.txt"}"#,
            ),
            ("reply.txt", b"hello {{cookie.x}}"),
        ]);

        let finding = one(&dir);
        assert_eq!(finding.kind, "unknown_template_source");
        assert!(
            finding.message.contains("{{cookie.x}}")
                && finding.message.contains("`response_file` 'reply.txt'"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn a_file_body_without_the_templating_optin_is_content_not_a_lie() {
        // `template` unset means the `{{…}}` is served verbatim. That is a
        // deliberate choice Mimic documents; the validator has no opinion.
        let dir = mocks_dir(&[
            (
                "raw.json",
                br#"{"method":"GET","path":"/raw","status":200,"response_file":"reply.txt"}"#,
            ),
            ("reply.txt", b"hello {{cookie.x}}"),
        ]);

        assert!(findings(&dir, None).is_empty());
    }

    #[test]
    fn a_binary_file_body_is_never_scanned_for_templates() {
        // Opted in, but `.bin` is served as `application/octet-stream` — the
        // server passes the bytes straight through, so does the validator.
        let dir = mocks_dir(&[
            (
                "bin.json",
                br#"{"method":"GET","path":"/bin","status":200,
                     "template":true,"response_file":"reply.bin"}"#,
            ),
            ("reply.bin", b"\x00\x01{{cookie.x}}\xff"),
        ]);

        assert!(findings(&dir, None).is_empty());
    }

    #[test]
    fn unknown_sources_in_sequence_step_responses_are_checked_too() {
        let dir = mocks_dir(&[(
            "steps.json",
            br#"{"method":"POST","path":"/steps","status":200,"response":{},
                 "sequence":[
                    {"status":200,"response":{"q":"{{query.q}}","who":"{{profile.name}}"}}
                 ]}"#,
        )]);

        let finding = one(&dir);
        assert!(
            finding.message.contains("{{profile.name}}")
                && finding.message.contains("sequence step 1"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn an_activated_tag_no_mock_declares_is_a_warning() {
        let dir = mocks_dir(&[(
            "users.json",
            br#"{"method":"GET","path":"/users","status":200,"response":{},"tags":["advanced"]}"#,
        )]);

        let found = findings(&dir, Some(&tags(["advanced"])));
        assert!(
            found.is_empty(),
            "declared tags are never reported: {:?}",
            found
        );

        let typo = findings(&dir, Some(&tags(["happy-path"])));
        assert_eq!(typo.len(), 1, "{:?}", typo);
        assert_eq!(typo[0].kind, "unknown_active_tag");
        assert_eq!(typo[0].severity, Severity::Warning);
        assert!(
            typo[0].message.contains("happy-path") && typo[0].message.contains("serves nothing"),
            "{}",
            typo[0].message
        );
    }

    // ------------------------------------------------------------------
    // Exit codes, CLI, and the two report shapes
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Exit codes, CLI, and the two report shapes
    // ------------------------------------------------------------------

    fn report_with(problems: Vec<Finding>) -> Report {
        Report {
            directory: "mocks".to_string(),
            files: 27,
            problems,
        }
    }

    #[test]
    fn the_exit_code_is_one_for_errors_and_for_strict_warnings() {
        let clean = report_with(vec![]);
        assert_eq!(exit_code(&clean, false), 0);
        assert_eq!(exit_code(&clean, true), 0);

        let warned = report_with(vec![Finding {
            file: "mocks/x.json".to_string(),
            severity: Severity::Warning,
            kind: "duplicate_route",
            message: "duplicate GET /x".to_string(),
        }]);
        assert_eq!(
            exit_code(&warned, false),
            0,
            "warnings alone are advisories"
        );
        assert_eq!(exit_code(&warned, true), 1, "--strict makes them fatal");

        let errored = report_with(vec![Finding {
            file: "mocks/x.json".to_string(),
            severity: Severity::Error,
            kind: "invalid_status",
            message: "status 1000 is not a valid HTTP status code".to_string(),
        }]);
        assert_eq!(exit_code(&errored, false), 1);
        assert_eq!(exit_code(&errored, true), 1);
    }

    #[test]
    fn the_text_report_aligns_columns_and_always_sums_up() {
        let report = report_with(vec![
            Finding {
                file: "mocks/checkout.json".to_string(),
                severity: Severity::Error,
                kind: "invalid_status",
                message: "status 1000 is not a valid HTTP status code".to_string(),
            },
            Finding {
                file: "mocks/x.json".to_string(),
                severity: Severity::Warning,
                kind: "duplicate_route",
                message: "duplicate GET /x".to_string(),
            },
        ]);

        let text = render_text(&report, false);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one row per problem, then the summary");

        // The severity word starts at the same column on every row: the
        // alignment is the whole point of the columns.
        let error_col = lines[0].find("error").unwrap();
        let warning_col = lines[1].find("warning").unwrap();
        assert_eq!(
            error_col, warning_col,
            "severity column must align:\n{}",
            text
        );

        assert!(
            lines[2].contains("2 problem(s) in 27 mock file(s): 1 error(s), 1 warning(s)"),
            "{}",
            lines[2]
        );
    }

    #[test]
    fn the_clean_text_report_is_the_summary_alone() {
        let report = report_with(vec![]);
        let text = render_text(&report, false);
        assert!(!text.contains('\n'), "one line, not a banner:\n{}", text);
        assert!(
            text.starts_with("0 problem(s) in 27 mock file(s):"),
            "{}",
            text
        );
    }

    #[test]
    fn the_json_report_is_a_stable_shape() {
        fn findings_for_shape() -> Report {
            report_with(vec![
                Finding {
                    file: "mocks/checkout.json".to_string(),
                    severity: Severity::Error,
                    kind: "invalid_status",
                    message: "status 1000 is not a valid HTTP status code".to_string(),
                },
                Finding {
                    file: "mocks/x.json".to_string(),
                    severity: Severity::Warning,
                    kind: "unknown_active_tag",
                    message: "MIMIC_ACTIVE_TAGS activates tag 'happy-path'".to_string(),
                },
            ])
        }

        let first = render_json(&findings_for_shape(), true);
        let second = render_json(&findings_for_shape(), true);
        assert_eq!(
            first, second,
            "the same input renders byte-for-byte the same"
        );

        let parsed: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(parsed["directory"], "mocks");
        assert_eq!(parsed["files"], 27);
        assert_eq!(parsed["strict"], true);
        assert_eq!(parsed["errors"], 1);
        assert_eq!(parsed["warnings"], 1);
        let problems = parsed["problems"].as_array().unwrap();
        assert_eq!(problems.len(), 2);
        assert_eq!(
            problems[0],
            json!({
                "file": "mocks/checkout.json",
                "severity": "error",
                "kind": "invalid_status",
                "message": "status 1000 is not a valid HTTP status code",
            })
        );
        assert_eq!(problems[1]["kind"], "unknown_active_tag");

        // The text report under --strict says plainly that warnings were fatal.
        let text = render_text(&findings_for_shape(), true);
        assert!(
            text.contains("(--strict: warnings counted as errors)"),
            "{}",
            text
        );
    }

    // ------------------------------------------------------------------
    // The CLI
    // ------------------------------------------------------------------

    fn args<const N: usize>(values: [&str; N]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn the_cli_takes_one_positional_strict_and_a_format() {
        let options = parse_args(&args(["./mocks", "--strict", "--format", "json"])).unwrap();
        assert_eq!(options.dir, Some(PathBuf::from("./mocks")));
        assert!(options.strict);
        assert_eq!(options.format, Format::Json);

        let defaults = parse_args(&args([])).unwrap();
        assert_eq!(defaults.dir, None);
        assert!(!defaults.strict, "warnings must survive by default");
        assert_eq!(defaults.format, Format::Text);
    }

    #[test]
    fn the_cli_rejects_bad_usage_with_the_usage_text() {
        let cases: [Vec<String>; 4] = [
            args(["--bogus"]),
            args(["a", "b"]),
            args(["--format"]),
            args(["--format", "yaml"]),
        ];
        for (index, args) in cases.into_iter().enumerate() {
            let error = parse_args(&args).unwrap_err();
            let ValidateError::Usage(message) = error;
            assert!(
                !message.is_empty(),
                "case {} must say what was wrong",
                index
            );
        }

        // `-h` is not an error: an empty message means "print USAGE, exit 0".
        assert!(matches!(
            parse_args(&args(["-h"])),
            Err(ValidateError::Usage(message)) if message.is_empty()
        ));
        assert!(parse_args(&args(["--help"]))
            .unwrap_err()
            .to_string()
            .is_empty());
    }

    // ------------------------------------------------------------------
    // Helpers' small contract
    // ------------------------------------------------------------------

    #[test]
    fn a_message_the_loader_never_trims_is_kept_verbatim() {
        let error_for = |message: &str| LoadError {
            file: Path::new("mocks/x.json").to_path_buf(),
            element: None,
            message: message.to_string(),
        };
        assert_eq!(
            message_for_report(&error_for("totally custom error")),
            "totally custom error"
        );
        assert_eq!(
            message_for_report(&error_for("Empty method in mocks/x.json")),
            "mock declares an empty `method`"
        );
        assert_eq!(
            message_for_report(&error_for(
                "mocks/x.json sets both `response` and `response_file` (body.txt); use one or the other"
            )),
            "`response` and `response_file` (body.txt); use one or the other"
        );
        // An element error names `file#index` in both the column and the
        // embedded prefix it gets trimmed by.
        let element_error = LoadError {
            file: Path::new("mocks/orders.json").to_path_buf(),
            element: Some(2),
            message: "mocks/orders.json#2 sets both `response` and `response_file` (body.txt); use one or the other".to_string(),
        };
        assert_eq!(
            message_for_report(&element_error),
            "`response` and `response_file` (body.txt); use one or the other"
        );
        assert_eq!(
            display_path(Path::new("./mocks/x.json")),
            "mocks/x.json",
            "a leading ./ is presentation noise"
        );
    }
}
