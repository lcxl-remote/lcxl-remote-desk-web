//! Directory-only browsing uses deterministic pagination without synthetic entries.
use super::*;

pub(super) async fn list_directories(
    params: &FileListParams,
) -> Result<FileListResponse, DeskError> {
    if params.page_no < 1 || !(1..=1000).contains(&params.page_count) {
        return DeskError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "Directory page_no must be positive and page_count must be between 1 and 1000",
        );
    }
    let offset = (params.page_no - 1)
        .checked_mul(params.page_count)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| {
            DeskError::new_custom_error(DeskErrorCode::INVALID_PARAMS, "Directory page overflow")
        })?;
    let mut directories = Vec::new();
    #[cfg(target_os = "windows")]
    if params.path.is_empty() {
        directories = get_logical_driver_list()?;
    }
    if !cfg!(target_os = "windows") || !params.path.is_empty() {
        let path = if params.path.is_empty() {
            "/"
        } else {
            &params.path
        };
        let mut entries = fs::read_dir(path).await?;
        while let Some(entry) = entries.next_entry().await? {
            // Resolve directory links for browsing; assistant consent separately
            // validates the canonical identity and containment before granting access.
            if fs::metadata(entry.path()).await?.is_dir() {
                directories.push(FileInfo::new(entry.path())?);
            }
        }
    }
    directories.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    let total_count = directories.len() as i64;
    Ok(FileListResponse {
        file_info_list: directories
            .into_iter()
            .skip(offset)
            .take(params.page_count as usize)
            .collect(),
        total_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn filters_before_counting_and_paging_without_parent_entries() {
        let root = tempfile::tempdir().unwrap();
        for name in ["z", "a", "m"] {
            fs::create_dir(root.path().join(name)).await.unwrap();
        }
        for name in ["0.txt", "b.txt", "y.txt"] {
            fs::write(root.path().join(name), b"file").await.unwrap();
        }
        let mut params = FileListParams {
            path: root.path().to_string_lossy().into(),
            page_no: 1,
            page_count: 2,
            directories_only: true,
            ..Default::default()
        };
        let first = list_files(params.clone()).await.unwrap();
        assert_eq!(first.total_count, 3);
        assert_eq!(
            first
                .file_info_list
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "m"]
        );
        params.page_no = 2;
        let second = list_files(params.clone()).await.unwrap();
        assert_eq!(second.total_count, 3);
        assert_eq!(second.file_info_list[0].name, "z");
        assert_eq!(second.file_info_list.len(), 1);
        params.page_no = 3;
        assert!(
            list_files(params.clone())
                .await
                .unwrap()
                .file_info_list
                .is_empty()
        );
        params.page_no = i64::MAX;
        assert!(list_files(params.clone()).await.is_err());
        params.page_no = 0;
        assert!(list_files(params).await.is_err());
    }

    #[tokio::test]
    async fn files_only_is_empty_but_missing_directory_is_error() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), b"x").await.unwrap();
        let mut params = FileListParams {
            path: root.path().to_string_lossy().into(),
            page_no: 1,
            page_count: 100,
            directories_only: true,
            ..Default::default()
        };
        assert_eq!(list_files(params.clone()).await.unwrap().total_count, 0);
        params.path = root.path().join("absent").to_string_lossy().into();
        assert!(list_files(params).await.is_err());
    }
}
