// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// The approach to gathering licenses — resolving the dependency graph via
// krates, scanning crate sources for license texts with spdx detection,
// and deduplicating results — is based on cargo-about
// (https://github.com/EmbarkStudios/cargo-about) by Embark Studios,
// licensed under MIT OR Apache-2.0.

use std::collections::BTreeMap;
use std::{cmp, fmt};

use krates::cm;
use krates::{Utf8Path, Utf8PathBuf};
use serde::Serialize;
use spdx::detection as sd;
use spdx::{Expression, LicenseReq, Licensee};

const CONFIDENCE_THRESHOLD: f32 = 0.8;

const IGNORE_PRIVATE: bool = true;
const IGNORE_DEV_DEPENDENCIES: bool = true;
const IGNORE_BUILD_DEPENDENCIES: bool = true;
const IGNORE_TRANSITIVE_DEPENDENCIES: bool = false;

struct Crate(cm::Package);

impl Crate {
    fn get_license_expression(&self) -> LicenseInfo {
        if let Some(license_field) = &self.0.license {
            match Crate::parse_license_expression(license_field) {
                Ok(validated) => LicenseInfo::Expr(validated),
                Err(err) => {
                    tracing::error!("unable to parse license expression for '{self}': {err}");
                    LicenseInfo::Unknown
                }
            }
        } else {
            tracing::warn!("crate '{self}' doesn't have a license field");
            LicenseInfo::Unknown
        }
    }

    fn parse_license_expression(license: &str) -> Result<Expression, spdx::ParseError> {
        Expression::parse_mode(
            license,
            spdx::ParseMode {
                allow_deprecated: true,
                allow_imprecise_license_names: true,
                allow_slash_as_or_operator: false,
                allow_postfix_plus_on_gpl: true,
                allow_unknown: false,
            },
        )
    }
}

impl Ord for Crate {
    fn cmp(&self, o: &Self) -> cmp::Ordering {
        match self.0.name.cmp(&o.0.name) {
            cmp::Ordering::Equal => self.0.version.cmp(&o.0.version),
            o => o,
        }
    }
}

impl PartialOrd for Crate {
    fn partial_cmp(&self, o: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(o))
    }
}

impl Eq for Crate {}

impl PartialEq for Crate {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == cmp::Ordering::Equal
    }
}

impl From<cm::Package> for Crate {
    fn from(mut pkg: cm::Package) -> Self {
        // Fix the license field as cargo used to allow the invalid / separator
        if let Some(lf) = &mut pkg.license {
            *lf = lf.replace('/', " OR ");
        }

        Self(pkg)
    }
}

impl krates::KrateDetails for Crate {
    fn name(&self) -> &str {
        &self.0.name
    }

    fn version(&self) -> &krates::semver::Version {
        &self.0.version
    }
}

impl fmt::Display for Crate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.0.name, self.0.version)
    }
}

impl std::ops::Deref for Crate {
    type Target = cm::Package;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

type Krates = krates::Krates<Crate>;

fn get_all_crates(cargo_toml: &Utf8Path) -> Result<Krates, krates::Error> {
    let mut mdc = krates::Cmd::new();
    mdc.manifest_path(cargo_toml);

    let mut builder = krates::Builder::new();

    if IGNORE_BUILD_DEPENDENCIES {
        builder.ignore_kind(krates::DepKind::Build, krates::Scope::All);
    }

    if IGNORE_DEV_DEPENDENCIES {
        builder.ignore_kind(krates::DepKind::Dev, krates::Scope::All);
    }

    if IGNORE_TRANSITIVE_DEPENDENCIES {
        builder.ignore_kind(krates::DepKind::Normal, krates::Scope::NonWorkspace);
        builder.ignore_kind(krates::DepKind::Dev, krates::Scope::NonWorkspace);
        builder.ignore_kind(krates::DepKind::Build, krates::Scope::NonWorkspace);
    }

    builder.include_targets(std::iter::once((env!("TARGET"), vec![])));

    let graph = builder.build(mdc, |filtered: cm::Package| {
        tracing::debug!("filtered {} {}", filtered.name, filtered.version);
    })?;

    Ok(graph)
}

type LicenseStore = sd::Store;

fn load_license_store() -> Result<LicenseStore, Box<dyn std::error::Error>> {
    Ok(sd::Store::load_inline()?)
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum LicenseInfo {
    Expr(Expression),
    Unknown,
}

enum LicenseFileKind {
    /// The license file is the canonical text of the license
    Text(String),
    /// The file just has a license header
    Header,
}

struct LicenseFile {
    license_expr: Expression,
    confidence: f32,
    kind: LicenseFileKind,
}

impl Ord for LicenseFile {
    fn cmp(&self, o: &Self) -> cmp::Ordering {
        match self.license_expr.as_ref().cmp(o.license_expr.as_ref()) {
            cmp::Ordering::Equal => o
                .confidence
                .partial_cmp(&self.confidence)
                .expect("NaN encountered comparing license confidences"),
            ord => ord,
        }
    }
}

impl PartialOrd for LicenseFile {
    fn partial_cmp(&self, o: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(o))
    }
}

impl PartialEq for LicenseFile {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == cmp::Ordering::Equal
    }
}

impl Eq for LicenseFile {}

struct KrateLicense<'krate> {
    krate: &'krate Crate,
    lic_info: LicenseInfo,
    license_files: Vec<LicenseFile>,
}

fn walk_files(dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("failed to read directory '{current}': {e}");
                continue;
            }
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            let path = match Utf8PathBuf::from_path_buf(entry.path()) {
                Ok(pb) => pb,
                Err(e) => {
                    tracing::warn!("skipping path {}, not a valid utf-8 path", e.display());
                    continue;
                }
            };

            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                files.push(path);
            }
        }
    }

    files
}

fn scan_files(
    root_dir: &Utf8Path,
    scanner: &sd::scan::Scanner<'_>,
    threshold: f32,
) -> Vec<LicenseFile> {
    walk_files(root_dir)
        .into_iter()
        .filter_map(|path| {
            let contents = read_file(&path)?;
            check_is_license_file(path, contents, scanner, threshold)
        })
        .collect()
}

fn read_file(path: &Utf8Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Err(ref e) if e.kind() == std::io::ErrorKind::InvalidData => {
            tracing::debug!("binary file '{path}' detected");
            None
        }
        Err(e) => {
            tracing::error!("failed to read '{path}': {e}");
            None
        }
        Ok(c) => Some(c),
    }
}

fn check_is_license_file(
    path: Utf8PathBuf,
    contents: String,
    scanner: &sd::scan::Scanner<'_>,
    threshold: f32,
) -> Option<LicenseFile> {
    match scan_text(&contents, scanner, threshold) {
        ScanResult::Header(ided) => {
            let license_expr = match Expression::parse(ided.id.name) {
                Ok(expr) => expr,
                Err(err) => {
                    tracing::error!(
                        "failed to parse license '{}' at {path:?} into a valid expression: {err}",
                        ided.id.name
                    );
                    return None;
                }
            };

            Some(LicenseFile {
                license_expr,
                confidence: ided.confidence,
                kind: LicenseFileKind::Header,
            })
        }
        ScanResult::Text(ided) => {
            let license_expr = match Expression::parse(ided.id.name) {
                Ok(expr) => expr,
                Err(err) => {
                    tracing::error!(
                        "failed to parse license '{}' at {path:?} into a valid expression: {err}",
                        ided.id.name
                    );
                    return None;
                }
            };

            Some(LicenseFile {
                license_expr,
                confidence: ided.confidence,
                kind: LicenseFileKind::Text(contents),
            })
        }
        ScanResult::UnknownId(id_str) => {
            tracing::error!("found unknown SPDX identifier '{id_str}' scanning '{path}'");
            None
        }
        ScanResult::LowLicenseChance(ided) => {
            tracing::debug!(
                "found '{}' scanning '{path}' but it only has a confidence score of {}",
                ided.id.name,
                ided.confidence,
            );
            None
        }
        ScanResult::NoLicense => None,
    }
}

struct Identified {
    confidence: f32,
    id: spdx::LicenseId,
}

enum ScanResult {
    Header(Identified),
    Text(Identified),
    UnknownId(String),
    LowLicenseChance(Identified),
    NoLicense,
}

fn scan_text(contents: &str, strat: &sd::scan::Scanner<'_>, threshold: f32) -> ScanResult {
    let text = spdx::detection::TextData::new(contents);
    let lic_match = strat.scan(&text);

    let Some(identified) = lic_match.license else {
        return ScanResult::NoLicense;
    };

    let lic_id = match spdx::license_id(identified.name) {
        Some(id) => Identified {
            confidence: lic_match.score,
            id,
        },
        None => return ScanResult::UnknownId(identified.name.to_owned()),
    };

    use spdx::detection::LicenseType;

    if lic_match.score >= threshold {
        match identified.kind {
            LicenseType::Header => ScanResult::Header(lic_id),
            LicenseType::Original => ScanResult::Text(lic_id),
            LicenseType::Alternate => {
                panic!("Alternate license detected")
            }
        }
    } else {
        ScanResult::LowLicenseChance(lic_id)
    }
}

fn gather_licenses<'k>(krates: &'k Krates, store: &LicenseStore) -> Vec<KrateLicense<'k>> {
    use rayon::prelude::*;

    let min_threshold = (CONFIDENCE_THRESHOLD - 0.5).max(0.1);

    let scanner = sd::scan::Scanner::new(store)
        .confidence_threshold(min_threshold)
        .optimize(false)
        .max_passes(1);

    let mut licensed_krates: Vec<_> = krates
        .krates()
        .par_bridge()
        .filter_map(|krate| {
            // Skip private/workspace crates
            if IGNORE_PRIVATE
                && let Some(publish) = &krate.publish
                && publish.is_empty()
            {
                tracing::debug!("ignoring private crate '{krate}'");
                return None;
            }

            let lic_info = krate.get_license_expression();
            let root_path = krate.manifest_path.parent().unwrap();

            let mut license_files = scan_files(root_path, &scanner, CONFIDENCE_THRESHOLD);

            // Condense each license down to the best candidate if
            // multiple are found
            license_files.sort();
            let mut last_expr = None;
            license_files.retain(|lf| {
                let dominated = last_expr.as_ref() == Some(&lf.license_expr);
                last_expr = Some(lf.license_expr.clone());
                !dominated
            });

            Some(KrateLicense {
                krate,
                lic_info,
                license_files,
            })
        })
        .collect();

    licensed_krates.sort_by(|a, b| a.krate.cmp(b.krate));
    licensed_krates
}

/// For an OR expression like "MIT OR Apache-2.0", pick the minimal set of
/// licenses to satisfy the expression. Actual license policy validation
/// is handled by `cargo deny`.
fn pick_licenses(expr: &Expression) -> Vec<LicenseReq> {
    let accepted: Vec<Licensee> = expr
        .requirements()
        .filter_map(|r| {
            r.req
                .license
                .id()
                .map(|id| Licensee::parse(id.name).unwrap())
        })
        .collect();

    expr.minimized_requirements(&accepted).unwrap_or_default()
}

/// For crates without a `license` field, synthesize requirements from
/// scanned license files.
fn synthesize_from_files(files: &[LicenseFile]) -> Vec<LicenseReq> {
    let mut reqs = Vec::new();
    for lf in files {
        for req in lf.license_expr.requirements() {
            if !reqs.contains(&req.req) {
                reqs.push(req.req.clone());
            }
        }
    }
    reqs
}

fn effective_licenses(kl: &KrateLicense<'_>) -> Vec<LicenseReq> {
    match &kl.lic_info {
        LicenseInfo::Expr(expr) => pick_licenses(expr),
        LicenseInfo::Unknown => {
            if kl.license_files.is_empty() {
                tracing::warn!(
                    "unable to determine license for '{}': no `license` specified, and no license files were found",
                    kl.krate
                );
                Vec::new()
            } else {
                synthesize_from_files(&kl.license_files)
            }
        }
    }
}

#[derive(Clone, Serialize)]
struct UsedBy {
    #[serde(rename = "crate")]
    krate: UsedByCrate,
}

#[derive(Clone, Serialize)]
struct UsedByCrate {
    name: String,
    version: String,
    repository: Option<String>,
}

#[derive(Clone, Serialize)]
struct License {
    name: String,
    id: String,
    first_of_kind: bool,
    text: String,
    used_by: Vec<UsedBy>,
}

#[derive(Serialize)]
struct LicenseSet {
    count: usize,
    name: String,
    id: String,
}

#[derive(Serialize)]
struct LicenseList {
    overview: Vec<LicenseSet>,
    licenses: Vec<License>,
}

fn generate(nfos: &[KrateLicense<'_>]) -> LicenseList {
    let mut licenses_map: BTreeMap<String, BTreeMap<String, License>> = BTreeMap::new();

    for krate_license in nfos {
        let reqs = effective_licenses(krate_license);

        for license_req in &reqs {
            let spdx::LicenseItem::Spdx { id, .. } = license_req.license else {
                tracing::warn!(
                    "{license_req} has no license file for crate '{}'",
                    krate_license.krate
                );
                continue;
            };

            // Try to find actual license text from scanned files
            let license_text = krate_license
                .license_files
                .iter()
                .find_map(|lf| {
                    if !lf
                        .license_expr
                        .evaluate(|ereq| ereq.license.id() == Some(id))
                    {
                        return None;
                    }

                    match &lf.kind {
                        LicenseFileKind::Text(text) => Some(text.clone()),
                        LicenseFileKind::Header => None,
                    }
                })
                .unwrap_or_else(|| {
                    tracing::debug!(
                        "unable to find text for license '{license_req}' for crate '{}', falling back to canonical text",
                        krate_license.krate
                    );
                    id.text().to_owned()
                });

            let used_by = UsedBy {
                krate: UsedByCrate {
                    name: krate_license.krate.name.clone(),
                    version: krate_license.krate.version.to_string(),
                    repository: krate_license.krate.repository.clone(),
                },
            };

            let entry = licenses_map.entry(id.full_name.to_owned()).or_default();

            let lic = entry
                .entry(license_text.clone())
                .or_insert_with(|| License {
                    name: id.full_name.to_owned(),
                    id: id.name.to_owned(),
                    text: license_text,
                    used_by: Vec::new(),
                    first_of_kind: false,
                });
            lic.used_by.push(used_by);
        }
    }

    let mut licenses: Vec<_> = licenses_map
        .into_iter()
        .flat_map(|(_, v)| v.into_values())
        .collect();

    for lic in &mut licenses {
        lic.used_by
            .sort_by(|a, b| a.krate.name.len().cmp(&b.krate.name.len()));
    }

    licenses.sort_by(|a, b| a.id.cmp(&b.id));

    let mut overview_map: BTreeMap<&str, LicenseSet> = BTreeMap::new();

    for lic in &mut licenses {
        let ls = overview_map.entry(&lic.id).or_insert_with(|| {
            lic.first_of_kind = true;
            LicenseSet {
                count: 0,
                name: lic.name.clone(),
                id: lic.id.clone(),
            }
        });
        ls.count += lic.used_by.len();
    }

    let mut overview: Vec<_> = overview_map.into_values().collect();
    overview.sort_by(|a, b| a.name.cmp(&b.name));

    LicenseList { overview, licenses }
}

/// Gathers all dependency licenses and returns a JSON string.
pub fn generate_json(manifest_path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let manifest_path = Utf8PathBuf::from(manifest_path);
    if !manifest_path.exists() {
        return Err(format!("manifest path '{manifest_path}' does not exist").into());
    }

    let krates = get_all_crates(&manifest_path)?;
    let store = load_license_store()?;
    let summary = gather_licenses(&krates, &store);
    let list = generate(&summary);

    Ok(serde_json::to_string_pretty(&list)?)
}
