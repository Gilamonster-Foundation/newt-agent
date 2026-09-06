use super::*;

#[test]
fn install_scrybe_resolves_the_binary_to_an_absolute_path() {
    // scrybe smart-install: with the binary present on PATH, it is registered
    // by ABSOLUTE path so the server survives later PATH changes.
    let sb = sandbox();
    let bin = sb.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let scrybe_bin = bin.join("scrybe-mcp-server");
    std::fs::write(&scrybe_bin, "#!/bin/sh\n").unwrap();

    newt(&sb)
        .env("PATH", &bin)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Installed MCP server 'scrybe'"))
        .stdout(predicate::str::contains("Scrybe Markdown editor"))
        .stdout(predicate::str::contains("Resolved command to"))
        .stdout(predicate::str::contains("newt doctor"));

    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers.len(), 1);
    let entry = &cfg.mcp_servers[0];
    assert_eq!(entry.name, "scrybe");
    assert_eq!(entry.command.as_deref(), Some(scrybe_bin.to_str().unwrap()));
    assert_eq!(entry.args, vec!["stdio"]);
    assert!(entry.enabled);
}

#[test]
fn install_scrybe_without_the_binary_hints_pip() {
    // The bundled scrybe entry with NO binary anywhere is a hard error naming
    // the pip package — the "special relationship" that removes setup friction.
    let sb = sandbox();
    let empty = sb.home.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    newt(&sb)
        .env("PATH", &empty)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pip install scrybe.ai"));
    // Nothing was registered.
    assert!(!sb.config_dir.join("config.toml").exists());
}

#[test]
fn install_unknown_name_lists_the_available_catalog() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "install", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ghost"))
        .stderr(predicate::str::contains("available: scrybe"));
    assert!(!sb.config_dir.join("config.toml").exists());
}

#[test]
fn catalog_drop_ins_layer_user_over_bundled_and_project_over_user() {
    let sb = sandbox();
    // Pin an empty PATH: a user/project drop-in that overrides scrybe keeps its
    // own bare command when the binary is absent (only the BUNDLED scrybe entry
    // hard-fails with a pip hint), so this exercises catalog layering cleanly.
    let empty = sb.home.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    // User drop-in overrides the bundled scrybe entry.
    std::fs::write(
        sb.config_dir.join("mcp-catalog.toml"),
        "[[servers]]\nname = \"scrybe\"\ncommand = \"scrybe-user\"\nargs = [\"stdio\"]\n",
    )
    .unwrap();
    newt(&sb)
        .env("PATH", &empty)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .success();
    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers[0].command.as_deref(), Some("scrybe-user"));
    newt(&sb)
        .args(["mcp", "remove", "scrybe"])
        .assert()
        .success();

    // Project drop-in overrides the user drop-in.
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    std::fs::write(
        sb.cwd.join(".newt").join("mcp-catalog.toml"),
        "[[servers]]\nname = \"scrybe\"\ncommand = \"scrybe-proj\"\nargs = [\"stdio\"]\n",
    )
    .unwrap();
    newt(&sb)
        .env("PATH", &empty)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .success();
    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers[0].command.as_deref(), Some("scrybe-proj"));
}

#[test]
fn malformed_catalog_drop_in_fails_install_loudly() {
    let sb = sandbox();
    std::fs::write(sb.config_dir.join("mcp-catalog.toml"), "not toml [").unwrap();
    newt(&sb)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("mcp-catalog.toml"));
    assert!(!sb.config_dir.join("config.toml").exists());
}

#[test]
fn broken_catalog_drop_in_entry_fails_install_naming_the_file() {
    let sb = sandbox();
    // Parses fine, but a stdio server with no command can never connect.
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    std::fs::write(
        sb.cwd.join(".newt").join("mcp-catalog.toml"),
        "[[servers]]\nname = \"half\"\ndescription = \"broken on purpose\"\n",
    )
    .unwrap();
    newt(&sb)
        .args(["mcp", "install", "half"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("half"))
        .stderr(predicate::str::contains("mcp-catalog.toml"))
        .stderr(predicate::str::contains("command"));
    assert!(!sb.config_dir.join("config.toml").exists());
}
