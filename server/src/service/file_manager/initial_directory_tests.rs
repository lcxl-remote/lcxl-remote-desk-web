use super::*;

fn initial_request() -> FileListParams {
    FileListParams {
        page_no: 1,
        page_count: 100,
        prefer_user_home: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn initial_listing_returns_home_contents_and_effective_path() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("home-file.txt"), b"home")
        .await
        .unwrap();
    let path = home.path().to_str().unwrap().to_owned();
    let response = list_files_with_home(initial_request(), Some(path.clone()))
        .await
        .unwrap();
    assert_eq!(response.path, path);
    assert!(
        response
            .file_info_list
            .iter()
            .any(|entry| entry.name == "home-file.txt")
    );
    assert!(
        response
            .file_info_list
            .iter()
            .any(|entry| entry.name == "..")
    );
}

#[tokio::test]
async fn missing_or_unreadable_home_falls_back_to_existing_root_entry() {
    let root = tempfile::tempdir().unwrap();
    let not_directory = root.path().join("file.txt");
    fs::write(&not_directory, b"file").await.unwrap();
    for home in [None, Some(root.path().join("missing")), Some(not_directory)] {
        let response = list_files_with_home(
            initial_request(),
            home.map(|path| path.to_str().unwrap().to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(response.path, "");
    }
}

#[tokio::test]
async fn explicit_paths_and_root_navigation_never_redirect_to_home() {
    let home = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let home_path = Some(home.path().to_str().unwrap().to_owned());
    let mut params = initial_request();
    params.path = directory.path().to_str().unwrap().to_owned();
    let response = list_files_with_home(params.clone(), home_path.clone())
        .await
        .unwrap();
    assert_eq!(response.path, params.path);
    params.path = directory
        .path()
        .join("missing")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        list_files_with_home(params, home_path.clone())
            .await
            .is_err()
    );
    let mut root_params = initial_request();
    root_params.prefer_user_home = false;
    assert_eq!(
        list_files_with_home(root_params.clone(), home_path.clone())
            .await
            .unwrap()
            .path,
        ""
    );
    root_params.directories_only = true;
    // Some root entries cannot be stat'ed (for example macOS special links).
    // Preserve the directory picker's existing result, including such errors.
    let expected = list_files_at_path(root_params.clone())
        .await
        .map(|response| response.path)
        .map_err(|error| error.to_string());
    assert_eq!(
        list_files_with_home(root_params, home_path)
            .await
            .map(|response| response.path)
            .map_err(|error| error.to_string()),
        expected
    );
}

#[tokio::test]
async fn directory_only_listing_reports_its_effective_path() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("directory")).await.unwrap();
    fs::write(home.path().join("file.txt"), b"file")
        .await
        .unwrap();
    let path = home.path().to_str().unwrap().to_owned();
    let mut params = initial_request();
    params.directories_only = true;
    let response = list_files_with_home(params, Some(path.clone()))
        .await
        .unwrap();
    assert_eq!(response.path, path);
    assert_eq!(response.total_count, 1);
    assert_eq!(response.file_info_list[0].name, "directory");
}
