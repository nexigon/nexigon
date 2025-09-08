use anyhow::anyhow;
use anyhow::bail;

use nexigon_api::types::repositories::RepositoryAssetId;
use nexigon_api::types::repositories::ResolvePackageVersionAssetByPathAction;
use nexigon_client::ClientExecutor;

pub struct AssetPath {
    repository: String,
    package: String,
    tag: String,
    filename: String,
}

pub fn parse_asset_path(path: &str) -> anyhow::Result<AssetPath> {
    let mut parts_iter = path.split('/');
    let repository = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing repository"))?
        .to_owned();
    let package = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing package"))?
        .to_owned();
    let tag = parts_iter
        .next()
        .ok_or_else(|| anyhow!("missing version tag"))?
        .to_owned();
    let filename = parts_iter.collect::<Vec<_>>().join("/");
    Ok(AssetPath {
        repository,
        package,
        tag,
        filename,
    })
}

pub async fn resolve_asset(
    executor: &mut ClientExecutor,
    asset: &str,
) -> anyhow::Result<RepositoryAssetId> {
    if asset.starts_with("repo_a_") {
        return Ok(asset.parse()?);
    }
    let path = parse_asset_path(asset)?;
    let output = executor
        .execute(ResolvePackageVersionAssetByPathAction::new(
            path.repository,
            path.package,
            path.tag,
            path.filename,
        ))
        .await??;
    match output {
        nexigon_api::types::repositories::ResolvePackageVersionAssetByPathOutput::Found(output) => {
            Ok(output.asset_id)
        }
        nexigon_api::types::repositories::ResolvePackageVersionAssetByPathOutput::NotFound => {
            bail!("package version asset {asset} not found")
        }
    }
}
