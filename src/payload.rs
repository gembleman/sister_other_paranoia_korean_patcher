use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unity_asset_binary::asset::{SerializedFile, SerializedFileParser};
use unity_asset_binary::bundle::{BundleLoadOptions, BundleParser};
use unity_asset_binary::shared_bytes::SharedBytes;
use unity_asset_core::{
    AssetLoadBudget, DigestV1, SourceId, SourceKind, VerifiedSourceImage, WorkspaceId,
};
use unity_asset_write::PackingPolicy;
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload, LogicalArtifactName,
};
use unity_asset_write::bundle::{BundleArtifactEntry, BundleWriter};
use unity_asset_write::object::{
    SerializedObjectEncoder, UnsafeRawObjectAcknowledgement, UnsafeRawObjectReplacement,
};
use unity_asset_write::serialized_file::{
    SerializedFileEdits, SerializedFileSource, SerializedFileWriter,
};

#[derive(RustEmbed)]
#[folder = "payloads/"]
struct EmbeddedPayloads;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub files: Vec<FilePatch>,
}

#[derive(Debug, Deserialize)]
pub struct FilePatch {
    pub path: String,
    pub kind: FileKind,
    #[serde(default)]
    pub objects: Vec<ObjectPatch>,
    #[serde(default)]
    pub ranges: Vec<RangePatch>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Bundle,
    Serialized,
    Raw,
}

#[derive(Debug, Deserialize)]
pub struct ObjectPatch {
    pub path_id: i64,
    #[serde(default)]
    pub asset_name: Option<String>,
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
    #[serde(default, rename = "name")]
    pub name: Option<String>,
    #[serde(default)]
    pub original_sha256: String,
    pub target_sha256: String,
    pub parts: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct RangePatch {
    pub offset: u64,
    #[serde(default)]
    pub original_sha256: String,
    pub target_sha256: String,
    pub parts: Vec<String>,
}

pub fn load_manifest() -> Result<&'static Manifest, String> {
    static MANIFEST: OnceLock<Result<Manifest, String>> = OnceLock::new();

    MANIFEST
        .get_or_init(|| {
            let asset = EmbeddedPayloads::get("manifest.json")
                .ok_or("내장 payload manifest를 찾을 수 없습니다.")?;
            let manifest: Manifest = serde_json::from_slice(asset.data.as_ref())
                .map_err(|e| format!("payload manifest 파싱 실패: {e}"))?;
            if manifest.version != 1 {
                return Err(format!("지원하지 않는 payload 버전: {}", manifest.version));
            }
            if manifest.files.is_empty() {
                return Err(
                    "내장 패치 payload가 비어 있습니다. payload 생성 후 다시 빌드하세요.".into(),
                );
            }
            Ok(manifest)
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn digest_hex(digest: impl AsRef<[u8]>) -> String {
    let digest = digest.as_ref();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

fn sha256(bytes: &[u8]) -> String {
    digest_hex(Sha256::digest(bytes))
}

fn payload_bytes(parts: &[String], expected: &str) -> Result<Vec<u8>, String> {
    let mut embedded_parts = Vec::with_capacity(parts.len());
    let mut total_len = 0usize;
    for name in parts {
        let key = if name.starts_with("blobs/") {
            Cow::Borrowed(name.as_str())
        } else {
            Cow::Owned(format!("blobs/{name}"))
        };
        let part = EmbeddedPayloads::get(&key)
            .ok_or_else(|| format!("내장 payload blob이 없습니다: {key}"))?;
        total_len = total_len
            .checked_add(part.data.len())
            .ok_or("payload 크기가 주소 공간을 초과합니다.")?;
        embedded_parts.push(part);
    }
    let mut bytes = Vec::with_capacity(total_len);
    let mut hasher = Sha256::new();
    for part in embedded_parts {
        hasher.update(part.data.as_ref());
        bytes.extend_from_slice(part.data.as_ref());
    }
    let actual = digest_hex(hasher.finalize());
    if !expected.is_empty() && !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "payload 무결성 오류: 예상 {expected}, 실제 {actual}"
        ));
    }
    Ok(bytes)
}

fn payload_len(parts: &[String], expected: &str) -> Result<usize, String> {
    let mut total_len = 0usize;
    let mut hasher = Sha256::new();
    for name in parts {
        let key = if name.starts_with("blobs/") {
            Cow::Borrowed(name.as_str())
        } else {
            Cow::Owned(format!("blobs/{name}"))
        };
        let part = EmbeddedPayloads::get(&key)
            .ok_or_else(|| format!("내장 payload blob이 없습니다: {key}"))?;
        total_len = total_len
            .checked_add(part.data.len())
            .ok_or("payload 크기가 주소 공간을 초과합니다.")?;
        hasher.update(part.data.as_ref());
    }
    let actual = digest_hex(hasher.finalize());
    if !expected.is_empty() && !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "payload 무결성 오류: 예상 {expected}, 실제 {actual}"
        ));
    }
    Ok(total_len)
}

pub fn files_for(manifest: &Manifest) -> &[FilePatch] {
    &manifest.files
}

fn source_payload(
    bytes: Arc<[u8]>,
    kind: SourceKind,
    local: u128,
) -> Result<ArtifactPayload, String> {
    let workspace = WorkspaceId::from_u128(0x534f_5000_0000_0000_0000_0000_0000_0001)
        .map_err(|e| e.to_string())?;
    let source = SourceId::new(workspace, kind, local).map_err(|e| e.to_string())?;
    let image = VerifiedSourceImage::verify(kind, bytes);
    ArtifactPayload::source_backed(source, image).map_err(|e| e.to_string())
}

fn build_edits<'a>(
    file: &SerializedFile,
    patches: impl Iterator<Item = (&'a ObjectPatch, i64)>,
    restore_file: Option<&SerializedFile>,
) -> Result<(SerializedFileEdits, usize), String> {
    let mut edits = SerializedFileEdits::new();
    let mut budget = AssetLoadBudget::default();
    let mut changed = 0;
    for (patch, path_id) in patches {
        let object = file
            .find_object(path_id)
            .ok_or_else(|| format!("PathID {path_id}를 찾지 못했습니다."))?;
        let current = file.object_bytes(object).map_err(|e| e.to_string())?;
        let desired = if let Some(original) = restore_file {
            let source_object = original
                .find_object(path_id)
                .ok_or_else(|| format!("백업에서 PathID {path_id}를 찾지 못했습니다."))?;
            original
                .object_bytes(source_object)
                .map_err(|e| e.to_string())?
                .to_vec()
        } else {
            payload_bytes(&patch.parts, &patch.target_sha256)?
        };
        if current == desired {
            continue;
        }
        if restore_file.is_none()
            && !patch.original_sha256.is_empty()
            && !sha256(current).eq_ignore_ascii_case(&patch.original_sha256)
        {
            return Err(format!(
                "PathID {}의 원본 무결성이 일치하지 않습니다. 다른 게임 버전이거나 외부 수정이 있습니다.",
                path_id
            ));
        }
        let encoded = SerializedObjectEncoder::new(file, path_id)
            .map_err(|e| e.to_string())?
            .encode_unsafe_raw(
                UnsafeRawObjectReplacement::new(
                    DigestV1::hash_bytes(current),
                    desired,
                    UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
                ),
                &mut budget,
            )
            .map_err(|e| e.to_string())?;
        edits
            .try_insert_encoded_object(encoded, &mut budget)
            .map_err(|e| e.to_string())?;
        changed += 1;
    }
    Ok((edits, changed))
}

fn class_id(type_name: Option<&str>) -> Option<i32> {
    match type_name? {
        "GameObject" => Some(1),
        "Texture2D" => Some(28),
        "TextAsset" => Some(49),
        "Material" => Some(21),
        "MonoBehaviour" => Some(114),
        "Font" => Some(128),
        "Sprite" => Some(213),
        _ => None,
    }
}

/// PathID is only a hint.  On a rebuilt Addressables bundle, locate the same
/// unchanged object by its logical selector and content digest.
fn resolve_object(file: &SerializedFile, patch: &ObjectPatch) -> Result<i64, String> {
    let expected_class = class_id(patch.type_name.as_deref());
    let digest_matches = |bytes: &[u8]| {
        let digest = sha256(bytes);
        digest.eq_ignore_ascii_case(&patch.original_sha256)
            || digest.eq_ignore_ascii_case(&patch.target_sha256)
    };
    let selector_matches = |class: i32| expected_class.is_none_or(|expected| expected == class);

    if let Some(object) = file.find_object(patch.path_id)
        && selector_matches(object.class_id())
        && digest_matches(file.object_bytes(object).map_err(|e| e.to_string())?)
    {
        return Ok(patch.path_id);
    }

    let mut budget = AssetLoadBudget::default();
    let mut matches = Vec::new();
    for handle in file.object_handles() {
        if !selector_matches(handle.class_id()) {
            continue;
        }
        if let Some(expected_name) = patch.name.as_deref() {
            let Ok(Some(actual_name)) = handle.peek_name(&mut budget) else {
                continue;
            };
            if actual_name != expected_name {
                continue;
            }
        }
        let Some(object) = file.find_object(handle.path_id()) else {
            continue;
        };
        if digest_matches(file.object_bytes(object).map_err(|e| e.to_string())?) {
            matches.push(handle.path_id());
        }
    }
    match matches.as_slice() {
        [path_id] => Ok(*path_id),
        [] => Err(format!(
            "대상 객체를 찾지 못했거나 원문이 변경되었습니다: {} / {} (PathID 힌트 {})",
            patch.type_name.as_deref().unwrap_or("unknown"),
            patch.name.as_deref().unwrap_or("unnamed"),
            patch.path_id
        )),
        _ => Err(format!(
            "대상 객체 후보가 {}개라 안전하게 선택할 수 없습니다: {} / {}",
            matches.len(),
            patch.type_name.as_deref().unwrap_or("unknown"),
            patch.name.as_deref().unwrap_or("unnamed")
        )),
    }
}

fn write_artifact(
    batch_result: unity_asset_write::artifact::PreparedArtifactSet,
    output_path: &Path,
) -> Result<(), String> {
    let file = File::create(output_path).map_err(|e| e.to_string())?;
    let mut output = BufWriter::with_capacity(1024 * 1024, file);
    batch_result
        .outputs()
        .next()
        .ok_or("생성된 출력 artifact가 없습니다.")?
        .artifact()
        .stream_verified_to(&mut output)
        .map_err(|e| e.to_string())?;
    output.flush().map_err(|e| e.to_string())?;
    output.get_ref().sync_all().map_err(|e| e.to_string())
}

fn patch_serialized(
    current: Arc<[u8]>,
    patches: &[ObjectPatch],
    backup: Option<Arc<[u8]>>,
    output_path: Option<&Path>,
) -> Result<usize, String> {
    let file = SerializedFileParser::from_shared_range(
        SharedBytes::from_arc(Arc::clone(&current)),
        0..current.len(),
    )
    .map_err(|e| e.to_string())?;
    let original = backup
        .map(|bytes| {
            let len = bytes.len();
            SerializedFileParser::from_shared_range(SharedBytes::from_arc(bytes), 0..len)
        })
        .transpose()
        .map_err(|e| e.to_string())?;
    let resolved = patches
        .iter()
        .map(|patch| resolve_object(&file, patch).map(|path_id| (patch, path_id)))
        .collect::<Result<Vec<_>, _>>()?;
    let (edits, changed) = build_edits(&file, resolved.into_iter(), original.as_ref())?;
    let Some(output_path) = output_path.filter(|_| changed > 0) else {
        return Ok(changed);
    };
    let payload = source_payload(current, SourceKind::SerializedFile, 1)?;
    let source = SerializedFileSource::whole(&payload).map_err(|e| e.to_string())?;
    let mut artifact_budget =
        ArtifactBudget::new(ArtifactLimits::default()).map_err(|e| e.to_string())?;
    let mut inspect = AssetLoadBudget::default();
    let mut declaration = ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspect)
        .map_err(|e| e.to_string())?;
    let slot = declaration
        .declare_output(LogicalArtifactName::new("resources.assets").map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let mut batch = declaration.seal_output_names().map_err(|e| e.to_string())?;
    let root = SerializedFileWriter::prepare(&mut batch, &file, &edits, Some(source))
        .map_err(|e| e.to_string())?;
    batch.bind_output(slot, root).map_err(|e| e.to_string())?;
    write_artifact(batch.finish().map_err(|e| e.to_string())?, output_path)?;
    Ok(changed)
}

fn patch_bundle(
    current: Vec<u8>,
    patches: &[ObjectPatch],
    backup: Option<Vec<u8>>,
    output_path: Option<&Path>,
) -> Result<usize, String> {
    let bundle_options = || {
        let mut options = BundleLoadOptions::complete();
        // 메인 번들은 완전 해제 시 기본 2 GiB 한도를 약간 넘는다.
        options.max_memory = Some(4 * 1024 * 1024 * 1024);
        options.max_unityfs_block_cache_memory = Some(4 * 1024 * 1024 * 1024);
        options
    };
    let bundle = BundleParser::from_bytes_with_options(current, bundle_options())
        .map_err(|e| e.to_string())?;
    let original_bundle = backup
        .map(|bytes| BundleParser::from_bytes_with_options(bytes, bundle_options()))
        .transpose()
        .map_err(|e| e.to_string())?;

    let mut by_asset: HashMap<usize, Vec<(&ObjectPatch, i64)>> = HashMap::new();
    for patch in patches {
        let mut found = Vec::new();
        for (index, asset) in bundle.assets.iter().enumerate() {
            if patch
                .asset_name
                .as_ref()
                .is_some_and(|name| bundle.asset_names[index] != *name)
            {
                continue;
            }
            if let Ok(path_id) = resolve_object(asset, patch) {
                found.push((index, path_id));
            }
        }
        let (asset_index, path_id) = match found.as_slice() {
            [(asset_index, path_id)] => (*asset_index, *path_id),
            [] => {
                return Err(format!(
                    "번들에서 대상 객체를 찾지 못했습니다: PathID 힌트 {}",
                    patch.path_id
                ));
            }
            _ => {
                return Err(format!(
                    "번들에서 대상 객체 후보가 여러 개입니다: PathID 힌트 {}",
                    patch.path_id
                ));
            }
        };
        by_asset
            .entry(asset_index)
            .or_default()
            .push((patch, path_id));
    }

    // 변경 여부를 먼저 판정해 이미 적용된 큰 번들을 불필요하게 재패킹하지 않는다.
    let mut edits_by_asset = HashMap::new();
    let mut changed = 0;
    for (asset_index, asset_patches) in &by_asset {
        let asset_name = &bundle.asset_names[*asset_index];
        let original_asset = original_bundle.as_ref().and_then(|b| {
            b.asset_names
                .iter()
                .position(|name| name == asset_name)
                .map(|index| &b.assets[index])
        });
        let (edits, count) = build_edits(
            &bundle.assets[*asset_index],
            asset_patches.iter().copied(),
            original_asset,
        )?;
        if count > 0 {
            edits_by_asset.insert(*asset_index, edits);
            changed += count;
        }
    }
    let Some(output_path) = output_path.filter(|_| changed > 0) else {
        return Ok(changed);
    };

    let asset_indices: HashMap<&str, usize> = bundle
        .asset_names
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    let mut payloads = Vec::with_capacity(bundle.files.len());
    for (index, file) in bundle.files.iter().enumerate() {
        let bytes = Arc::<[u8]>::from(bundle.extract_file_data(file).map_err(|e| e.to_string())?);
        let kind = if asset_indices.contains_key(file.name.as_str()) {
            SourceKind::SerializedFile
        } else {
            SourceKind::StreamedResource
        };
        payloads.push(source_payload(bytes, kind, (index + 1) as u128)?);
    }

    let mut artifact_budget =
        ArtifactBudget::new(ArtifactLimits::default()).map_err(|e| e.to_string())?;
    let mut inspect = AssetLoadBudget::default();
    let mut declaration = ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspect)
        .map_err(|e| e.to_string())?;
    let slot = declaration
        .declare_output(LogicalArtifactName::new("patched.bundle").map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let mut batch = declaration.seal_output_names().map_err(|e| e.to_string())?;
    let mut handles = Vec::with_capacity(bundle.files.len());
    for (file_index, file_info) in bundle.files.iter().enumerate() {
        if let Some(asset_index) = asset_indices.get(file_info.name.as_str()).copied()
            && let Some(edits) = edits_by_asset.get(&asset_index)
        {
            let source =
                SerializedFileSource::whole(&payloads[file_index]).map_err(|e| e.to_string())?;
            handles.push(
                SerializedFileWriter::prepare(
                    &mut batch,
                    &bundle.assets[asset_index],
                    edits,
                    Some(source),
                )
                .map_err(|e| e.to_string())?,
            );
            continue;
        }
        handles.push(
            batch
                .prepare_verbatim_source(&payloads[file_index])
                .map_err(|e| e.to_string())?,
        );
    }
    let mut entries = Vec::with_capacity(bundle.nodes.len());
    let mut file_index = 0;
    for node in &bundle.nodes {
        if node.is_directory() {
            entries.push(
                BundleArtifactEntry::empty_directory_from_node(node).map_err(|e| e.to_string())?,
            );
        } else if node.is_deleted() {
            entries.push(BundleArtifactEntry::deleted_from_node(node).map_err(|e| e.to_string())?);
        } else {
            entries.push(
                BundleArtifactEntry::file(&batch, &node.name, node.flags, handles[file_index])
                    .map_err(|e| e.to_string())?,
            );
            file_index += 1;
        }
    }
    let root =
        BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Preserve)
            .map_err(|e| e.to_string())?;
    batch.bind_output(slot, root).map_err(|e| e.to_string())?;
    write_artifact(batch.finish().map_err(|e| e.to_string())?, output_path)?;
    Ok(changed)
}

fn patch_raw(
    target_path: &Path,
    patches: &[RangePatch],
    backup_path: Option<&Path>,
    output_path: Option<&Path>,
) -> Result<usize, String> {
    let mut current = File::open(target_path).map_err(|e| e.to_string())?;
    let mut backup = backup_path
        .map(File::open)
        .transpose()
        .map_err(|e| e.to_string())?;
    let mut changed_patches = Vec::with_capacity(patches.len());
    for patch in patches {
        let desired = if let Some(original) = backup.as_mut() {
            let len = payload_len(&patch.parts, &patch.target_sha256)?;
            let mut bytes = vec![0; len];
            original
                .seek(SeekFrom::Start(patch.offset))
                .and_then(|_| original.read_exact(&mut bytes))
                .map_err(|e| format!("백업 range 읽기 실패: {e}"))?;
            bytes
        } else {
            payload_bytes(&patch.parts, &patch.target_sha256)?
        };
        let mut current_bytes = vec![0; desired.len()];
        current
            .seek(SeekFrom::Start(patch.offset))
            .and_then(|_| current.read_exact(&mut current_bytes))
            .map_err(|e| format!("patch range 읽기 실패: {e}"))?;
        if current_bytes == desired {
            continue;
        }
        if backup_path.is_none()
            && !patch.original_sha256.is_empty()
            && !sha256(&current_bytes).eq_ignore_ascii_case(&patch.original_sha256)
        {
            return Err(format!(
                "{} 오프셋의 원본 무결성이 일치하지 않습니다.",
                patch.offset
            ));
        }
        changed_patches.push((patch, desired));
    }
    if let Some(output_path) = output_path.filter(|_| !changed_patches.is_empty()) {
        fs::copy(target_path, output_path).map_err(|e| e.to_string())?;
        let mut output = OpenOptions::new()
            .write(true)
            .open(output_path)
            .map_err(|e| e.to_string())?;
        for (patch, desired) in &changed_patches {
            output
                .seek(SeekFrom::Start(patch.offset))
                .and_then(|_| output.write_all(desired))
                .map_err(|e| format!("patch range 쓰기 실패: {e}"))?;
        }
        output.sync_all().map_err(|e| e.to_string())?;
    }
    Ok(changed_patches.len())
}

fn replace_file(temp: &Path, target: &Path) -> Result<(), String> {
    fs::rename(temp, target).map_err(|e| format!("파일 교체 실패: {e}"))
}

fn resolve_target(data_dir: &Path, file: &FilePatch, restore: bool) -> Result<PathBuf, String> {
    let declared = data_dir.join(PathBuf::from(&file.path));
    if declared.is_file() || restore || file.kind != FileKind::Bundle {
        return Ok(declared);
    }

    let parent = declared.parent().ok_or_else(|| {
        format!(
            "대상 경로에 상위 디렉터리가 없습니다: {}",
            declared.display()
        )
    })?;
    let mut matches = Vec::new();
    for entry in fs::read_dir(parent).map_err(|e| format!("번들 디렉터리 읽기 실패: {e}"))?
    {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "bundle")
        {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        if patch_bundle(bytes, &file.objects, None, None).is_ok() {
            matches.push(path);
        }
    }
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(format!(
            "번들 이름이 변경되었으며 논리적으로 일치하는 새 번들을 찾지 못했습니다: {}",
            file.path
        )),
        _ => Err(format!(
            "논리적으로 일치하는 새 번들이 {}개라 안전하게 선택할 수 없습니다: {}",
            matches.len(),
            file.path
        )),
    }
}

fn patch_to_output(
    target: &Path,
    file: &FilePatch,
    backup: Option<&Path>,
    output: Option<&Path>,
) -> Result<usize, String> {
    if file.kind == FileKind::Raw {
        return patch_raw(target, &file.ranges, backup, output);
    }
    if !file.ranges.is_empty() {
        return Err("bundle/serialized 파일의 range 패치는 지원하지 않습니다.".into());
    }
    match file.kind {
        FileKind::Bundle => patch_bundle(
            fs::read(target).map_err(|e| e.to_string())?,
            &file.objects,
            backup
                .map(fs::read)
                .transpose()
                .map_err(|e| e.to_string())?,
            output,
        ),
        FileKind::Serialized => patch_serialized(
            Arc::<[u8]>::from(fs::read(target).map_err(|e| e.to_string())?),
            &file.objects,
            backup
                .map(|path| fs::read(path).map(Arc::<[u8]>::from))
                .transpose()
                .map_err(|e| e.to_string())?,
            output,
        ),
        FileKind::Raw => unreachable!(),
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut input = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(digest_hex(hasher.finalize()))
}

#[derive(Debug, Serialize, Deserialize)]
struct PatchState {
    version: u32,
    entries: Vec<PatchStateEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PatchStateEntry {
    manifest_path: String,
    actual_path: String,
    original_sha256: String,
    patched_sha256: String,
}

struct PreparedPatch {
    target: PathBuf,
    temp: PathBuf,
    rollback: PathBuf,
    backup: PathBuf,
    count: usize,
    original_sha256: String,
    patched_sha256: String,
}

#[derive(Default)]
struct SessionFiles(Vec<PathBuf>);

impl Drop for SessionFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// Prepare every output first, then commit as one session.  Any commit error
/// restores all files already replaced from their content-addressed backups.
pub fn apply_transaction(
    data_dir: &Path,
    files: &[FilePatch],
    restore: bool,
    dry_run: bool,
    log: &dyn Fn(String),
) -> Result<usize, String> {
    let mut session_files = SessionFiles::default();
    let state_path = data_dir.join(".sop_patch_state.json");
    let old_state = if restore {
        let bytes =
            fs::read(&state_path).map_err(|e| format!("복원 상태 파일을 읽을 수 없습니다: {e}"))?;
        Some(
            serde_json::from_slice::<PatchState>(&bytes)
                .map_err(|e| format!("복원 상태 파일이 손상되었습니다: {e}"))?,
        )
    } else {
        None
    };
    let mut prepared = Vec::with_capacity(files.len());
    for (index, file) in files.iter().enumerate() {
        let target = if let Some(state) = old_state.as_ref() {
            let entry = state
                .entries
                .iter()
                .find(|entry| entry.manifest_path == file.path)
                .ok_or_else(|| format!("복원 정보가 없습니다: {}", file.path))?;
            data_dir.join(&entry.actual_path)
        } else {
            resolve_target(data_dir, file, false)?
        };
        if !target.is_file() {
            return Err(format!("대상 파일이 없습니다: {}", target.display()));
        }
        let relative = target
            .strip_prefix(data_dir)
            .map_err(|_| "대상 파일이 게임 폴더 밖에 있습니다.".to_string())?;
        let (original_sha256, patched_sha256, backup) = if let Some(state) = old_state.as_ref() {
            let entry = state
                .entries
                .iter()
                .find(|entry| entry.manifest_path == file.path)
                .unwrap();
            let current = sha256_file(&target)?;
            if !current.eq_ignore_ascii_case(&entry.patched_sha256) {
                return Err(format!(
                    "패치 후 게임 파일이 업데이트되거나 수정되어 자동 복원할 수 없습니다: {}",
                    target.display()
                ));
            }
            let backup = data_dir
                .join(".sop_backups")
                .join(&entry.original_sha256)
                .join(relative);
            (
                entry.original_sha256.clone(),
                entry.patched_sha256.clone(),
                backup,
            )
        } else {
            let original = sha256_file(&target)?;
            let backup = data_dir.join(".sop_backups").join(&original).join(relative);
            (original, String::new(), backup)
        };
        if restore && !backup.is_file() {
            return Err(format!("버전별 원본 백업이 없습니다: {}", backup.display()));
        }
        log(format!("  검사: {}", relative.display()));
        if dry_run {
            let count = if restore {
                1
            } else {
                patch_to_output(&target, file, None, None)?
            };
            prepared.push(PreparedPatch {
                target,
                temp: PathBuf::new(),
                rollback: PathBuf::new(),
                backup,
                count,
                original_sha256,
                patched_sha256,
            });
            continue;
        }
        let temp = target.with_file_name(format!(
            ".{}.sop_stage_{index}",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        let rollback = target.with_file_name(format!(
            ".{}.sop_rollback_{index}",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        session_files.0.push(temp.clone());
        session_files.0.push(rollback.clone());
        if temp.exists() {
            fs::remove_file(&temp).map_err(|e| e.to_string())?;
        }
        if rollback.exists() {
            fs::remove_file(&rollback).map_err(|e| e.to_string())?;
        }
        let count = if restore {
            fs::copy(&backup, &temp).map_err(|e| format!("복원 staging 실패: {e}"))?;
            1
        } else {
            let count = patch_to_output(&target, file, None, Some(&temp))?;
            if count > 0 {
                let remaining = patch_to_output(&temp, file, None, None)?;
                if remaining != 0 {
                    return Err(format!(
                        "staging 출력 재검증 실패: {}개 대상이 적용되지 않았습니다 ({})",
                        remaining,
                        target.display()
                    ));
                }
            }
            count
        };
        let output_hash = if count > 0 {
            sha256_file(&temp)?
        } else {
            original_sha256.clone()
        };
        prepared.push(PreparedPatch {
            target,
            temp,
            rollback,
            backup,
            count,
            original_sha256,
            patched_sha256: output_hash,
        });
    }
    let total = prepared.iter().map(|item| item.count).sum();
    if dry_run || total == 0 {
        return Ok(total);
    }
    if !restore && prepared.iter().any(|item| item.count == 0) {
        return Err("일부 파일만 이미 패치된 상태입니다. 게임 파일 무결성 검사를 실행한 뒤 다시 시도하세요.".into());
    }

    for item in &prepared {
        if item.count == 0 {
            continue;
        }
        if let Some(parent) = item.backup.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("백업 폴더 생성 실패: {e}"))?;
        }
        if !item.backup.exists() {
            fs::copy(&item.target, &item.backup)
                .map_err(|e| format!("버전별 백업 생성 실패: {e}"))?;
        }
        fs::copy(&item.target, &item.rollback).map_err(|e| format!("롤백 사본 생성 실패: {e}"))?;
    }
    let mut committed = Vec::new();
    for (index, item) in prepared.iter().enumerate() {
        if item.count == 0 {
            continue;
        }
        if let Err(error) = replace_file(&item.temp, &item.target) {
            for committed_index in committed.into_iter().rev() {
                let prior: &PreparedPatch = &prepared[committed_index];
                let _ = fs::copy(&prior.rollback, &prior.target);
            }
            for pending in &prepared {
                let _ = fs::remove_file(&pending.temp);
                let _ = fs::remove_file(&pending.rollback);
            }
            return Err(format!(
                "트랜잭션 커밋 실패, 적용된 파일을 롤백했습니다: {error}"
            ));
        }
        committed.push(index);
    }
    let state_result = if restore {
        fs::remove_file(&state_path).map_err(|e| format!("패치 상태 제거 실패: {e}"))
    } else {
        let entries = files
            .iter()
            .zip(&prepared)
            .map(|(file, item)| PatchStateEntry {
                manifest_path: file.path.clone(),
                actual_path: item
                    .target
                    .strip_prefix(data_dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
                original_sha256: item.original_sha256.clone(),
                patched_sha256: item.patched_sha256.clone(),
            })
            .collect();
        let state = serde_json::to_vec_pretty(&PatchState {
            version: 1,
            entries,
        })
        .map_err(|e| e.to_string())?;
        fs::write(&state_path, state).map_err(|e| format!("패치 상태 저장 실패: {e}"))
    };
    if let Err(error) = state_result {
        for committed_index in committed.into_iter().rev() {
            let prior: &PreparedPatch = &prepared[committed_index];
            let _ = fs::copy(&prior.rollback, &prior.target);
        }
        return Err(format!("상태 저널 처리 실패, 파일을 롤백했습니다: {error}"));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::{FileKind, load_manifest, payload_bytes, payload_len, replace_file};
    use std::fs;

    #[test]
    fn payload_length_matches_reconstructed_bytes() {
        let manifest = load_manifest().unwrap();
        let patch = &manifest.files[0].objects[0];
        let bytes = payload_bytes(&patch.parts, &patch.target_sha256).unwrap();

        assert_eq!(
            payload_len(&patch.parts, &patch.target_sha256).unwrap(),
            bytes.len()
        );
    }

    #[test]
    fn every_object_has_a_logical_type_selector() {
        let manifest = load_manifest().unwrap();
        let objects = manifest
            .files
            .iter()
            .flat_map(|file| file.objects.iter())
            .collect::<Vec<_>>();

        assert!(!objects.is_empty());
        assert!(objects.iter().all(|patch| patch.type_name.is_some()));
        assert!(manifest.files.iter().any(|file| file.kind == FileKind::Raw));
    }

    #[test]
    fn replace_file_overwrites_existing_target() {
        let dir = std::env::temp_dir().join(format!(
            "sop_korean_patcher_replace_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let temp = dir.join("temp");
        let target = dir.join("target");
        fs::write(&temp, b"patched").unwrap();
        fs::write(&target, b"original").unwrap();

        replace_file(&temp, &target).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"patched");
        assert!(!temp.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
