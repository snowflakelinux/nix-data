use crate::{
    cache::{NixPkgList, NixosPkgList, StrOrVec},
    CACHEDIR,
};
use anyhow::{anyhow, Context, Result};
use log::{debug, info};
use sqlx::{migrate::MigrateDatabase, Row, Sqlite, SqlitePool};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, Write},
    path::Path,
    process::{Command, Stdio},
};

use super::{channel, flakes};

/// Downloads the latest `packages.json` for the system from the NixOS cache and returns the path to the file.
/// Will only work on NixOS systems.
pub async fn nixospkgs() -> Result<String> {
    let versionout = Command::new("nixos-version").output()?;
    let numver = &String::from_utf8(versionout.stdout)?[0..5];
    let version = if numver == "22.11" {
        "unstable"
    } else {
        numver
    };

    // If cache directory doesn't exist, create it
    if !std::path::Path::new(&*CACHEDIR).exists() {
        std::fs::create_dir_all(&*CACHEDIR)?;
    }

    let verurl = format!("https://channels.nixos.org/nixos-{}", version);
    let resp = reqwest::blocking::get(&verurl)?;
    let latestnixosver = resp
        .url()
        .path_segments()
        .context("No path segments found")?
        .last()
        .context("Last element not found")?
        .to_string();
    let latestnixosver = latestnixosver.strip_prefix("nixos-").unwrap_or(&latestnixosver);
    info!("latestnixosver: {}", latestnixosver);
    // Check if latest version is already downloaded
    if let Ok(prevver) = fs::read_to_string(&format!("{}/nixospkgs.ver", &*CACHEDIR)) {
        if prevver == latestnixosver && Path::new(&format!("{}/nixospkgs.db", &*CACHEDIR)).exists()
        {
            debug!("No new version of NixOS found");
            return Ok(format!("{}/nixospkgs.db", &*CACHEDIR));
        }
    }

    let url = format!(
        "https://channels.nixos.org/nixos-{}/packages.json.br",
        version
    );

    // Download file with reqwest blocking
    let client = reqwest::blocking::Client::builder().brotli(true).build()?;
    let resp = client.get(url).send()?;
    if resp.status().is_success() {
        // resp is pkgsjson
        let db = format!("sqlite://{}/nixospkgs.db", &*CACHEDIR);

        if Path::new(&format!("{}/nixospkgs.db", &*CACHEDIR)).exists() {
            fs::remove_file(&format!("{}/nixospkgs.db", &*CACHEDIR))?;
        }
        Sqlite::create_database(&db).await?;
        let pool = SqlitePool::connect(&db).await?;
        sqlx::query(
            r#"
                CREATE TABLE "pkgs" (
                    "attribute"	TEXT NOT NULL UNIQUE,
                    "system"	TEXT,
                    "pname"	TEXT,
                    "version"	TEXT,
                    PRIMARY KEY("attribute")
                )
                "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE "meta" (
                "attribute"	TEXT NOT NULL UNIQUE,
                "broken"	INTEGER,
                "insecure"	INTEGER,
                "unsupported"	INTEGER,
                "unfree"	INTEGER,
                "description"	TEXT,
                "longdescription"	TEXT,
                "homepage"	TEXT,
                "maintainers"	JSON,
                "position"	TEXT,
                "license"	JSON,
                "platforms"	JSON,
                FOREIGN KEY("attribute") REFERENCES "pkgs"("attribute"),
                PRIMARY KEY("attribute")
            )
                "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE UNIQUE INDEX "attributes" ON "pkgs" ("attribute")
            "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE UNIQUE INDEX "metaattributes" ON "meta" ("attribute")
            "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX "pnames" ON "pkgs" ("pname")
            "#,
        )
        .execute(&pool)
        .await?;

        let pkgjson: NixosPkgList =
            serde_json::from_reader(BufReader::new(resp)).expect("Failed to parse packages.json");

        let mut wtr = csv::Writer::from_writer(vec![]);
        for (pkg, data) in &pkgjson.packages {
            wtr.serialize((
                pkg,
                data.system.to_string(),
                data.pname.to_string(),
                data.version.to_string(),
            ))?;
        }
        let data = String::from_utf8(wtr.into_inner()?)?;
        let mut cmd = Command::new("sqlite3")
            .arg("-csv")
            .arg(&format!("{}/nixospkgs.db", &*CACHEDIR))
            .arg(".import '|cat -' pkgs")
            .stdin(Stdio::piped())
            .spawn()?;
        let cmd_stdin = cmd.stdin.as_mut().unwrap();
        cmd_stdin.write_all(data.as_bytes())?;
        let _status = cmd.wait()?;
        let mut metawtr = csv::Writer::from_writer(vec![]);
        for (pkg, data) in &pkgjson.packages {
            metawtr.serialize((
                pkg,
                if let Some(x) = data.meta.broken {
                    if x {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                },
                if let Some(x) = data.meta.insecure {
                    if x {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                },
                if let Some(x) = data.meta.unsupported {
                    if x {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                },
                if let Some(x) = data.meta.unfree {
                    if x {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                },
                data.meta.description.as_ref().map(|x| x.to_string()),
                data.meta.longdescription.as_ref().map(|x| x.to_string()),
                data.meta.homepage.as_ref().and_then(|x| match x {
                    StrOrVec::List(x) => x.first().map(|x| x.to_string()),
                    StrOrVec::Single(x) => Some(x.to_string()),
                }),
                data.meta
                    .maintainers
                    .as_ref()
                    .and_then(|x| match serde_json::to_string(x) {
                        Ok(x) => Some(x),
                        Err(_) => None,
                    }),
                data.meta.position.as_ref().map(|x| x.to_string()),
                data.meta
                    .license
                    .as_ref()
                    .and_then(|x| match serde_json::to_string(x) {
                        Ok(x) => Some(x),
                        Err(_) => None,
                    }),
                data.meta
                    .platforms
                    .as_ref()
                    .and_then(|x| match serde_json::to_string(x) {
                        Ok(x) => Some(x),
                        Err(_) => None,
                    }),
            ))?;
        }
        let metadata = String::from_utf8(metawtr.into_inner()?)?;
        let mut metacmd = Command::new("sqlite3")
            .arg("-csv")
            .arg(&format!("{}/nixospkgs.db", &*CACHEDIR))
            .arg(".import '|cat -' meta")
            .stdin(Stdio::piped())
            .spawn()?;
        let metacmd_stdin = metacmd.stdin.as_mut().unwrap();
        metacmd_stdin.write_all(metadata.as_bytes())?;
        let _status = metacmd.wait()?;
        // Write version downloaded to file
        File::create(format!("{}/nixospkgs.ver", &*CACHEDIR))?
            .write_all(latestnixosver.as_bytes())?;
    } else {
        return Err(anyhow!("Failed to download latest packages.json"));
    }

    Ok(format!("{}/nixospkgs.db", &*CACHEDIR))
}

/// Downloads the latest 'options.json' for the system from the NixOS cache and returns the path to the file.
/// Will only work on NixOS systems.
pub fn nixosoptions() -> Result<String> {
    let versionout = Command::new("nixos-version").output()?;
    let numver = &String::from_utf8(versionout.stdout)?[0..5];
    let version = if numver == "22.11" {
        "unstable"
    } else {
        numver
    };

    // If cache directory doesn't exist, create it
    if !std::path::Path::new(&*CACHEDIR).exists() {
        std::fs::create_dir_all(&*CACHEDIR)?;
    }

    let verurl = format!("https://channels.nixos.org/nixos-{}", version);
    let resp = reqwest::blocking::get(&verurl)?;
    let latestnixosver = resp
        .url()
        .path_segments()
        .context("No path segments found")?
        .last()
        .context("Last element not found")?
        .to_string();
    info!("latestnixosver: {}", latestnixosver);
    // Check if latest version is already downloaded
    if let Ok(prevver) = fs::read_to_string(&format!("{}/nixosoptions.ver", &*CACHEDIR)) {
        if prevver == latestnixosver
            && Path::new(&format!("{}/nixosoptions.json", &*CACHEDIR)).exists()
        {
            debug!("No new version of NixOS found");
            return Ok(format!("{}/nixosoptions.json", &*CACHEDIR));
        }
    }

    let url = format!(
        "https://channels.nixos.org/nixos-{}/options.json.br",
        version
    );

    // Download file with reqwest blocking
    let client = reqwest::blocking::Client::builder().brotli(true).build()?;
    let mut resp = client.get(url).send()?;
    if resp.status().is_success() {
        let mut out = File::create(&format!("{}/nixosoptions.json", &*CACHEDIR))?;
        resp.copy_to(&mut out)?;
        // Write version downloaded to file
        File::create(format!("{}/nixosoptions.ver", &*CACHEDIR))?
            .write_all(latestnixosver.as_bytes())?;
    } else {
        return Err(anyhow!("Failed to download latest options.json"));
    }

    Ok(format!("{}/nixosoptions.json", &*CACHEDIR))
}

pub(super) enum NixosType {
    Flake,
    Legacy,
}

pub(super) async fn getnixospkgs(
    paths: &[&str],
    nixos: NixosType,
) -> Result<HashMap<String, String>> {
    let pkgs = {
        let mut allpkgs: HashSet<String> = HashSet::new();
        for path in paths {
            if let Ok(filepkgs) = nix_editor::read::getarrvals(
                &fs::read_to_string(path)?,
                "environment.systemPackages",
            ) {
                let filepkgset = filepkgs.into_iter().collect::<HashSet<_>>();
                allpkgs = allpkgs.union(&filepkgset).map(|x| x.to_string()).collect();
            }
        }
        allpkgs
    };
    let pkgsdb = match nixos {
        NixosType::Flake => flakes::flakespkgs().await?,
        NixosType::Legacy => channel::legacypkgs().await?,
    };
    let mut out = HashMap::new();
    let pool = SqlitePool::connect(&format!("sqlite://{}", pkgsdb)).await?;
    for pkg in pkgs {
        let mut sqlout = sqlx::query(
            r#"
            SELECT pname, version FROM pkgs WHERE attribute = $1
            "#,
        )
        .bind(&pkg)
        .fetch_all(&pool)
        .await?;
        if sqlout.len() == 1 {
            let row = sqlout.pop().unwrap();
            let version: String = row.get("version");
            out.insert(pkg, version);
        }
    }
    Ok(out)
}

pub(super) async fn createdb(dbfile: &str, pkgjson: &NixPkgList) -> Result<()> {
    let db = format!("sqlite://{}", dbfile);
    if Path::new(dbfile).exists() {
        fs::remove_file(dbfile)?;
    }
    Sqlite::create_database(&db).await?;
    let pool = SqlitePool::connect(&db).await?;
    sqlx::query(
        r#"
            CREATE TABLE "pkgs" (
                "attribute"	TEXT NOT NULL UNIQUE,
                "pname"	TEXT,
                "version"	TEXT,
                PRIMARY KEY("attribute")
            )
            "#,
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        r#"
        CREATE UNIQUE INDEX "attributes" ON "pkgs" ("attribute")
        "#,
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        r#"
        CREATE INDEX "pnames" ON "pkgs" ("attribute")
        "#,
    )
    .execute(&pool)
    .await?;

    let mut wtr = csv::Writer::from_writer(vec![]);
    for (pkg, data) in &pkgjson.packages {
        wtr.serialize((pkg, data.pname.to_string(), data.version.to_string()))?;
    }
    let data = String::from_utf8(wtr.into_inner()?)?;
    let mut cmd = Command::new("sqlite3")
        .arg("-csv")
        .arg(&dbfile)
        .arg(".import '|cat -' pkgs")
        .stdin(Stdio::piped())
        .spawn()?;
    let cmd_stdin = cmd.stdin.as_mut().unwrap();
    cmd_stdin.write_all(data.as_bytes())?;
    let _status = cmd.wait()?;
    Ok(())
}
