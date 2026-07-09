//! Read-only tool bodies.
//!
//! Free-function versions of the `#[tool]`-annotated methods on
//! [`HolofsHandler`]. `lib.rs` delegates each `#[tool]` here so the
//! bodies live outside the macro-decorated impl block and the file
//! stays readable.

use rmcp::ErrorData;

use crate::util::{catalog_row, gw_err, hex_encode, kind_str, MAX_READ_TEXT_BYTES};
use crate::views::{
    CatalogRow, DiffIn, DiffLayerOut, DiffOut, HealthIndexOut, InspectIn, InspectLayerOut,
    InspectOut, InspectShardIn, InspectShardOut, InspectShardRow, ListCatalogIn, ListCatalogOut,
    NodeRow, ObjectHealthIn, ObjectHealthOut, ReadTextIn, ReadTextOut, SimilarIn, SimilarOut,
    SimilarRow,
};
use crate::HolofsHandler;

pub(crate) async fn list_catalog(
    h: &HolofsHandler,
    ListCatalogIn { prefix, recursive }: ListCatalogIn,
) -> Result<ListCatalogOut, ErrorData> {
    let prefix = prefix.unwrap_or_default();
    let rows: Vec<CatalogRow> = if recursive {
        // Walk the catalog snapshot ourselves so we get every
        // descendant — `list_dir` is intentionally one-level deep.
        let cat = h.gateway.catalog().read().await.clone();
        let needle = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        cat.entries
            .iter()
            .filter(|(k, _)| {
                if k.as_str() == prefix {
                    return false;
                }
                needle.is_empty() || k.starts_with(&needle)
            })
            .map(|(k, v)| catalog_row(k, v))
            .collect()
    } else {
        h.gateway
            .list_dir(&prefix)
            .await
            .map_err(gw_err)?
            .into_iter()
            .map(|(path, m)| catalog_row(&path, &m))
            .collect::<Vec<_>>()
    };
    Ok(ListCatalogOut {
        prefix,
        recursive,
        count: rows.len(),
        entries: rows,
    })
}

pub(crate) async fn read_object_text(
    h: &HolofsHandler,
    ReadTextIn { path }: ReadTextIn,
) -> Result<ReadTextOut, ErrorData> {
    use holofs_model::manifest::ObjectKind;
    let dec = h.gateway.decode_object(&path, None).await.map_err(gw_err)?;
    if dec.kind != ObjectKind::Text {
        return Err(ErrorData::invalid_params(
            format!(
                "object {path} is kind={:?} (not text); read_object_text refuses binary payloads",
                dec.kind
            ),
            None,
        ));
    }
    let total = dec.bytes.len() as u64;
    let truncated = dec.bytes.len() > MAX_READ_TEXT_BYTES;
    let slice = if truncated {
        &dec.bytes[..MAX_READ_TEXT_BYTES]
    } else {
        &dec.bytes[..]
    };
    let content = String::from_utf8_lossy(slice).to_string();
    Ok(ReadTextOut {
        path,
        truncated,
        total_bytes: total,
        content,
    })
}

pub(crate) async fn find_similar(
    h: &HolofsHandler,
    SimilarIn { path, scope }: SimilarIn,
) -> Result<SimilarOut, ErrorData> {
    use holofs_gateway::{SimilarScope, SimilarityMethod};
    let scope_str = scope.unwrap_or_else(|| "all".into());
    let parsed = SimilarScope::parse(&scope_str);
    let report = h.gateway.similar_to(&path, parsed).await.map_err(gw_err)?;
    let neighbors = report
        .neighbors
        .into_iter()
        .map(|m| SimilarRow {
            path: m.name,
            similarity_pct: m.similarity_pct,
            method: match m.method {
                SimilarityMethod::Jaccard => "Jaccard".into(),
                SimilarityMethod::DHash => "DHash".into(),
            },
        })
        .collect();
    Ok(SimilarOut {
        target: report.name,
        kind: kind_str(report.kind),
        scope: scope_str,
        neighbors,
    })
}

pub(crate) async fn get_cluster_health(h: &HolofsHandler) -> HealthIndexOut {
    let idx = h.gateway.health_index_data().await;
    let nodes: Vec<NodeRow> = idx
        .nodes
        .iter()
        .map(|n| NodeRow {
            idx: n.idx,
            addr: n.addr.clone(),
            zone: n.zone,
            admin_killed: n.admin_killed,
        })
        .collect();
    HealthIndexOut {
        n_live: idx.n_live,
        n_total: idx.n_total,
        nodes,
        objects: idx.objects,
    }
}

pub(crate) async fn get_object_health(
    h: &HolofsHandler,
    ObjectHealthIn { path }: ObjectHealthIn,
) -> Result<ObjectHealthOut, ErrorData> {
    use holofs_core::hash::hex;
    let oh = h.gateway.object_health(&path).await.map_err(gw_err)?;
    Ok(ObjectHealthOut {
        object_id: oh.object_id,
        name: oh.name,
        data_cid_hex: hex(&oh.data_cid),
        n_nodes: oh.n_nodes,
        n_live: oh.n_live,
        nlayers: oh.nlayers,
        channels: oh.channels,
        k: oh.k,
        current_resolution: oh.current_resolution,
    })
}

pub(crate) async fn diff_objects(
    h: &HolofsHandler,
    DiffIn {
        a,
        b,
        include_cells,
    }: DiffIn,
) -> Result<DiffOut, ErrorData> {
    let report = h.gateway.diff_chunks(&a, &b).await.map_err(gw_err)?;
    let layers = report
        .layers
        .into_iter()
        .map(|l| DiffLayerOut {
            channel: l.channel,
            layer: l.layer,
            n_common: l.n_common,
            n_total: l.n_total,
            cells: include_cells.then(|| l.cells.iter().map(|c| c.is_common).collect()),
        })
        .collect();
    Ok(DiffOut {
        a: report.name_a,
        b: report.name_b,
        kind: kind_str(report.kind),
        common: report.common,
        total: report.total,
        similarity_pct: report.similarity_pct,
        storage_saved_bytes: report.storage_saved_bytes,
        layers,
    })
}

pub(crate) async fn inspect_object(
    h: &HolofsHandler,
    InspectIn { path }: InspectIn,
) -> Result<InspectOut, ErrorData> {
    let info = h.gateway.inspect(&path).await.map_err(gw_err)?;
    let layers = info
        .layers
        .into_iter()
        .map(|l| InspectLayerOut {
            channel: l.channel,
            layer: l.layer,
            n_shards: l.n_shards,
            k_systematic: l.k_systematic,
            shards: l
                .shards
                .into_iter()
                .map(|s| InspectShardRow {
                    idx: s.idx,
                    node_idx: s.node_idx,
                    node_addr: s.node_addr,
                    is_systematic: s.is_systematic,
                })
                .collect(),
        })
        .collect();
    Ok(InspectOut {
        path: info.name,
        kind: kind_str(info.kind),
        channels: info.channels,
        nlayers: info.nlayers,
        k: info.k,
        layers,
    })
}

pub(crate) async fn inspect_shard(
    h: &HolofsHandler,
    InspectShardIn {
        path,
        channel,
        layer,
        idx,
        include_payload,
    }: InspectShardIn,
) -> Result<InspectShardOut, ErrorData> {
    match h
        .gateway
        .shard_payload(&path, channel, layer, idx)
        .await
        .map_err(gw_err)?
    {
        Some(sp) => Ok(InspectShardOut {
            path,
            channel,
            layer,
            idx,
            is_systematic: sp.is_systematic,
            hash_hex: sp.hash_hex,
            node_idx: sp.node_idx,
            node_addr: sp.node_addr,
            sym_len: sp.sym_len,
            coeffs: sp.coeffs,
            payload_hex: include_payload.then(|| hex_encode(&sp.payload)),
            fetch_error: None,
        }),
        // `None` means the node was unreachable / returned a bad
        // hash. Surface that explicitly so the caller can correlate
        // with health reports instead of guessing.
        None => Ok(InspectShardOut {
            path,
            channel,
            layer,
            idx,
            is_systematic: false,
            hash_hex: String::new(),
            node_idx: 0,
            node_addr: String::new(),
            sym_len: 0,
            coeffs: Vec::new(),
            payload_hex: None,
            fetch_error: Some("shard not retrievable from placement node".into()),
        }),
    }
}
