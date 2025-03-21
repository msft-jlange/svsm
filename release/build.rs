// SPDX-License-Identifier: MIT
//
// Copyright (c) 2025 SUSE LLC
//
// Author: Joerg Roedel <jroedel@suse.de>

use std::fs::OpenOptions;
use std::io::prelude::*;
use std::process::Command;
use std::string::String;

fn git_version() -> Result<String, ()> {
    let output = Command::new("git")
        .args(["describe", "--always", "--dirty=+"])
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }

    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut lines = stdout.lines();
    let first_line = lines.next().unwrap().trim();

    Ok(String::from(first_line))
}

fn write_version(version: String) {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open("src/git_version.rs")
        .unwrap();
    writeln!(&mut file, "// SPDX-License-Identifier: MIT\n//").unwrap();
    writeln!(
        &mut file,
        "// This file is automatically generated - Any changes will be overwritten\n//\n"
    )
    .unwrap();
    writeln!(&mut file, "pub const GIT_VERSION: &str = \"{}\";", version).unwrap();
}

fn main() {
    write_version(git_version().unwrap_or_default());
}
