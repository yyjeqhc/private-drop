use super::*;

fn write_skill(root: &Path, package: &str, name: &str, body: &str) {
    let package = root.join(package);
    fs::create_dir_all(package.join("references")).unwrap();
    fs::write(
        package.join(SKILL_DEFINITION_FILE),
        format!("---\nname: {name}\ndescription: configured test skill\n---\n{body}"),
    )
    .unwrap();
    fs::write(package.join("references/guide.md"), "line one\nline two\n").unwrap();
}

#[test]
fn configured_skill_scan_triggers_are_closed_and_identity_free() {
    assert_eq!(
        ConfiguredSkillScanTrigger::CatalogList.as_str(),
        "catalog_list"
    );
    assert_eq!(
        ConfiguredSkillScanTrigger::ExactReadResolution.as_str(),
        "exact_read_resolution"
    );
}

#[test]
fn empty_roots_preserve_empty_source() {
    let result = handle_configured_skill_roots_request(
        &SkillsConfig::default(),
        ConfiguredSkillRootsRequest::List,
    );
    let response: ConfiguredSkillRootsListResponse =
        serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap();
    assert!(response.skills.is_empty());
    assert!(response.diagnostics.is_empty());
}

#[test]
fn live_discovery_and_resource_read_observe_changes() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(temp.path(), "demo", "demo", "version one");
    let config = SkillsConfig {
        roots: vec![temp.path().to_path_buf()],
    };
    let first = discover(&config).unwrap();
    assert_eq!(first.skills.len(), 1);
    let id = first.skills[0].descriptor.skill_id.clone();
    let revision = first.skills[0].descriptor.definition_revision.clone();
    let listed = handle_configured_skill_roots_request(&config, ConfiguredSkillRootsRequest::List);
    let listed_text = listed.stdout.as_deref().unwrap();
    assert!(!listed_text.contains(temp.path().to_string_lossy().as_ref()));
    let definition =
        read_resource(&config, &id, SKILL_DEFINITION_FILE, 1, 20, Some(&revision)).unwrap();
    assert!(definition.text.contains("version one"));
    let resource =
        read_resource(&config, &id, "references/guide.md", 1, 20, Some(&revision)).unwrap();
    assert_eq!(resource.text, "line one\nline two");

    write_skill(temp.path(), "demo", "demo", "version two");
    let second = discover(&config).unwrap();
    assert_eq!(second.skills[0].descriptor.skill_id, id);
    assert_ne!(second.skills[0].descriptor.definition_revision, revision);
    assert_eq!(
        read_resource(&config, &id, SKILL_DEFINITION_FILE, 1, 20, Some(&revision)).unwrap_err(),
        "skill_definition_changed"
    );
    let fresh_revision = &second.skills[0].descriptor.definition_revision;
    let fresh = read_resource(
        &config,
        &id,
        SKILL_DEFINITION_FILE,
        1,
        20,
        Some(fresh_revision),
    )
    .unwrap();
    assert!(fresh.text.contains("version two"));
    let serialized = serde_json::to_string(&fresh).unwrap();
    assert!(!serialized.contains(temp.path().to_string_lossy().as_ref()));
}

#[test]
fn missing_root_is_a_bounded_path_free_diagnostic_not_an_empty_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing-live-skills");
    let result = handle_configured_skill_roots_request(
        &SkillsConfig {
            roots: vec![missing.clone()],
        },
        ConfiguredSkillRootsRequest::List,
    );
    assert_eq!(result.exit_code, Some(0));
    let stdout = result.stdout.as_deref().unwrap();
    assert!(!stdout.contains(missing.to_string_lossy().as_ref()));
    let response: ConfiguredSkillRootsListResponse = serde_json::from_str(stdout).unwrap();
    assert!(response.skills.is_empty());
    assert_eq!(
        response.diagnostics,
        vec!["configured_skill_root_not_found".to_string()]
    );
}

#[test]
fn same_package_in_two_roots_has_distinct_opaque_identity() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_skill(first.path(), "same", "same", "first");
    write_skill(second.path(), "same", "same", "second");
    let discovery = discover(&SkillsConfig {
        roots: vec![first.path().to_path_buf(), second.path().to_path_buf()],
    })
    .unwrap();
    assert_eq!(discovery.skills.len(), 2);
    assert_ne!(
        discovery.skills[0].descriptor.skill_id,
        discovery.skills[1].descriptor.skill_id
    );
    for skill in discovery.skills {
        assert!(!skill
            .descriptor
            .skill_id
            .contains(&first.path().to_string_lossy().as_ref()));
        assert!(!skill
            .descriptor
            .skill_id
            .contains(&second.path().to_string_lossy().as_ref()));
    }
}

#[test]
fn list_response_truncates_valid_unicode_descriptors_to_wire_budget() {
    let temp = tempfile::tempdir().unwrap();
    let description = "界".repeat(webcodex_core::skill_metadata::MAX_SKILL_DESCRIPTION_CHARS);
    for index in 0..MAX_CONFIGURED_SKILL_PACKAGES {
        let package = temp.path().join(format!("skill-{index:03}"));
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join(SKILL_DEFINITION_FILE),
            format!("---\nname: skill-{index:03}\ndescription: {description}\n---\nbody\n"),
        )
        .unwrap();
    }

    let result = handle_configured_skill_roots_request(
        &SkillsConfig {
            roots: vec![temp.path().to_path_buf()],
        },
        ConfiguredSkillRootsRequest::List,
    );
    assert_eq!(result.exit_code, Some(0));
    let stdout = result.stdout.as_deref().unwrap();
    assert!(stdout.len() <= CONFIGURED_SKILL_ROOTS_RESPONSE_MAX_BYTES);
    let response: ConfiguredSkillRootsListResponse = serde_json::from_str(stdout).unwrap();
    response.validate().unwrap();
    assert!(response.discovery_truncated);
    assert!(!response.skills.is_empty());
    assert!(response.skills.len() < MAX_CONFIGURED_SKILL_PACKAGES);
}

#[test]
fn configured_root_symlink_is_rejected() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let target = tempfile::tempdir().unwrap();
        write_skill(target.path(), "demo", "demo", "body");
        let holder = tempfile::tempdir().unwrap();
        let link = holder.path().join("skills-link");
        symlink(target.path(), &link).unwrap();
        let discovery = discover(&SkillsConfig { roots: vec![link] }).unwrap();
        assert!(discovery.skills.is_empty());
        assert!(discovery
            .diagnostics
            .iter()
            .any(|code| code == "configured_skill_root_link_not_allowed"));
    }
}

#[test]
fn traversal_and_symlink_escape_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(temp.path(), "demo", "demo", "body");
    let config = SkillsConfig {
        roots: vec![temp.path().to_path_buf()],
    };
    let skill = discover(&config).unwrap().skills.remove(0);
    assert_eq!(
        read_resource(
            &config,
            &skill.descriptor.skill_id,
            "../secret",
            1,
            20,
            None
        )
        .unwrap_err(),
        "skill_resource_path_invalid"
    );
    assert_eq!(
        read_resource(
            &config,
            &skill.descriptor.skill_id,
            "/etc/passwd",
            1,
            20,
            None
        )
        .unwrap_err(),
        "skill_resource_path_invalid"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.md"), "secret").unwrap();
        symlink(
            outside.path().join("secret.md"),
            temp.path().join("demo/references/escape.md"),
        )
        .unwrap();
        assert_eq!(
            read_resource(
                &config,
                &skill.descriptor.skill_id,
                "references/escape.md",
                1,
                20,
                None,
            )
            .unwrap_err(),
            "skill_resource_path_invalid"
        );
    }
}

#[test]
fn sensitive_package_names_are_not_discovered() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(temp.path(), ".git", "hidden", "must stay hidden");
    let discovery = discover(&SkillsConfig {
        roots: vec![temp.path().to_path_buf()],
    })
    .unwrap();
    assert!(discovery.skills.is_empty());
    assert_eq!(discovery.invalid_count, 1);
    assert!(discovery
        .diagnostics
        .iter()
        .any(|code| code == "sensitive_skill_definition"));
}

#[test]
fn malformed_and_oversized_definitions_are_bounded_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("bad")).unwrap();
    fs::write(temp.path().join("bad/SKILL.md"), "not frontmatter").unwrap();
    fs::create_dir_all(temp.path().join("huge")).unwrap();
    fs::write(
        temp.path().join("huge/SKILL.md"),
        vec![b'x'; MAX_SKILL_DEFINITION_BYTES + 1],
    )
    .unwrap();
    let discovery = discover(&SkillsConfig {
        roots: vec![temp.path().to_path_buf()],
    })
    .unwrap();
    assert_eq!(discovery.invalid_count, 2);
    assert!(discovery.diagnostics.len() <= MAX_CONFIGURED_SKILL_DIAGNOSTICS);
    assert!(discovery
        .diagnostics
        .iter()
        .any(|code| code == "skill_definition_too_large"));
}

#[test]
fn resource_reads_enforce_actual_byte_bound_and_preserve_range_metadata() {
    let temp = tempfile::tempdir().unwrap();
    write_skill(temp.path(), "demo", "demo", "body");
    let config = SkillsConfig {
        roots: vec![temp.path().to_path_buf()],
    };
    let skill = discover(&config).unwrap().skills.remove(0);
    let resource = temp.path().join("demo/references/guide.md");
    // Selecting a tiny range must still reject an oversized complete file.
    let mut bytes = vec![b'\n'; MAX_CONFIGURED_SKILL_RESOURCE_FILE_BYTES];
    fs::write(&resource, &bytes).unwrap();
    let read = read_resource(
        &config,
        &skill.descriptor.skill_id,
        "references/guide.md",
        2,
        1,
        None,
    )
    .unwrap();
    assert_eq!(read.file_bytes, bytes.len());
    assert_eq!(read.sha256, sha256_hex(&bytes));
    assert_eq!(read.total_lines, bytes.len());
    assert_eq!(read.returned_lines, 1);
    assert_eq!(read.next_start_line, Some(3));
    bytes.push(b'\n');
    fs::write(&resource, &bytes).unwrap();
    assert_eq!(
        read_resource(
            &config,
            &skill.descriptor.skill_id,
            "references/guide.md",
            2,
            1,
            None
        )
        .unwrap_err(),
        "skill_resource_too_large"
    );
    fs::write(&resource, [0xff]).unwrap();
    assert_eq!(
        read_resource(
            &config,
            &skill.descriptor.skill_id,
            "references/guide.md",
            1,
            1,
            None
        )
        .unwrap_err(),
        "skill_resource_unsupported_encoding"
    );
}
