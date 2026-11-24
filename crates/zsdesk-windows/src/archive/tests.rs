use super::*;

fn make_zip(path: &Path, entries: &[(&str, &[u8])]) -> Result<()> {
    let mut zip = ZipWriter::new(File::create(path)?);
    for (name, bytes) in entries {
        if name.ends_with('/') {
            zip.add_directory(*name, SimpleFileOptions::default())?;
        } else {
            zip.start_file(*name, SimpleFileOptions::default())?;
            zip.write_all(bytes)?;
        }
    }
    zip.finish()?;
    Ok(())
}

#[test]
fn extraction_uses_the_archive_root_and_keeps_existing_folders() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let zip = temp.path().join("资料.zip");
    for (entries, expected) in [
        (vec![("规程/一.txt", &b"a"[..])], "规程"),
        (vec![("规程/", &b""[..])], "规程 (2)"),
        (vec![("一.txt", &b"a"[..]), ("二.txt", b"b")], "资料"),
        (vec![("a/b.txt", &b"a"[..]), ("c/", b"")], "资料 (2)"),
        (vec![("a/b.txt", &b"a"[..]), ("c.txt", b"")], "资料 (3)"),
        (vec![], "资料 (4)"),
    ] {
        make_zip(&zip, &entries)?;
        assert_eq!(extract_here(&zip, temp.path())?, temp.path().join(expected));
    }
    assert_eq!(fs::read(temp.path().join("规程/一.txt"))?, b"a");
    Ok(())
}

#[test]
fn rejected_member_leaves_no_partial_extraction() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let zip = temp.path().join("test.zip");
    for name in [
        "../outside",
        "X:/path",
        "item:stream",
        "dir/NUL.txt",
        "dir/alias.",
    ] {
        make_zip(&zip, &[("valid.txt", b"ok"), (name, b"bad")])?;
        assert!(extract_here(&zip, temp.path()).is_err());
        assert_eq!(fs::read_dir(temp.path())?.count(), 1);
    }
    Ok(())
}

#[test]
fn full_selection_preserves_files_and_nested_empty_directories() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path().join("标准");
    fs::create_dir_all(dir.join("附录/空目录"))?;
    fs::write(dir.join("附录/细则.txt"), b"contents")?;
    for index in 0..20 {
        fs::write(dir.join(format!("规范／💡 {index}.pdf")), [index; 255])?;
    }
    let selected = fs::read_dir(&dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    let zip = compress_zip(&selected)?;
    assert_eq!(zip, dir.join("标准.zip"));
    assert_eq!(compress_zip(&selected)?, dir.join("标准 (2).zip"));
    let out = extract_here(&zip, temp.path())?;
    assert_eq!(out, temp.path().join("标准 (2)"));
    assert!(out.join("附录/空目录").is_dir());
    assert_eq!(fs::read(out.join("附录/细则.txt"))?, b"contents");
    for index in 0..20 {
        assert_eq!(
            fs::read(out.join(format!("规范／💡 {index}.pdf")))?,
            [index; 255]
        );
    }
    Ok(())
}

#[test]
fn tar_extraction_uses_the_same_layout_rules() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let archive = temp.path().join("bundle.tar");
    let mut writer = tar::Builder::new(File::create(&archive)?);
    let mut header = tar::Header::new_gnu();
    header.set_size(3);
    header.set_mode(0o644);
    writer.append_data(&mut header, "folder/标准.txt", &b"abc"[..])?;
    drop(writer.into_inner()?);
    let out = extract_here(&archive, temp.path())?;
    assert_eq!(out, temp.path().join("folder"));
    assert_eq!(fs::read(out.join("标准.txt"))?, b"abc");
    Ok(())
}

#[test]
#[ignore = "Reads an existing ZIP and compares its files with an external fixture directory"]
fn existing_zip_and_selected_files_round_trip() -> Result<()> {
    let folder =
        PathBuf::from(std::env::var_os("ZSDESK_ARCHIVE_SOURCE").context("fixture directory")?);
    let existing = PathBuf::from(std::env::var_os("ZSDESK_ARCHIVE_ZIP").context("reference zip")?);
    let temp = tempfile::tempdir()?;
    let extracted = extract_here(&existing, temp.path())?;
    let files = fs::read_dir(&folder)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    let mut selected = Vec::new();
    for file in &files {
        let local = extracted.join(file.file_name().unwrap());
        assert_eq!(fs::read(&local)?, fs::read(file)?);
        selected.push(local);
    }
    let result = compress_zip(&selected)?;
    let round_trip = extract_here(&result, temp.path())?;
    assert_eq!(fs::read_dir(&round_trip)?.count(), files.len());
    for file in files {
        assert_eq!(
            fs::read(round_trip.join(file.file_name().unwrap()))?,
            fs::read(file)?
        );
    }
    Ok(())
}
