use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::{Command, Child};

use anyhow::{Result, Error};
use fs_extra::{
    copy_items,
    dir::CopyOptions
};

use phf_codegen::Map;
use rayon::prelude::*;
use serde::Deserialize;

fn main() -> Result<()> {
    let languages = fs::read_to_string("src/languages.toml")?;
    let languages = toml::from_str::<Languages>(&languages)?.languages;

    if std::env::var("INKJET_REBUILD_LANGS").is_ok() {
        download_langs(&languages)?;
        generate_langs_module(&languages)?;
    }

    languages
        .par_iter()
        .for_each(Language::compile);

    Ok(())
}

fn download_langs(languages: &[Language]) -> Result<()> {
    fs::remove_dir_all("languages")?;
    fs::create_dir_all("languages/temp")?;
    
    languages
        .par_iter()
        .map(|lang| {
            (lang.download(), lang)
        })
        .try_for_each(|(child, lang)| -> Result<()> {
            child?.wait()?;

            let from = vec![
                format!("languages/temp/{}/src", lang.name),
                format!("languages/temp/{}/queries", lang.name)
            ];

            let to = format!("languages/{}", lang.name);
            
            fs::create_dir_all(format!("languages/{}", lang.name))?;
            copy_items(&from, to, &CopyOptions::new())?;

            Ok(())
        })?;
    
    fs::remove_dir_all("languages/temp")?;

    Ok(()) 
}

fn generate_langs_module(languages: &[Language]) -> Result<()> {
    let mut buffer = indoc::indoc!("
        #![allow(dead_code)]
        #![allow(clippy::items_after_test_module)]
        // This module is automatically generated by Inkjet.\n
    ").to_owned();

    let mut map = phf_codegen::Map::new();

    for lang in languages {
        lang.codegen(&mut buffer, &mut map);
    }

    let mut file = BufWriter::new(File::create("src/languages.rs")?);

    write!(
        &mut file,
        "{}",
        &buffer
    )?;


    write!(
        &mut file,
        "pub static LANG_MAP: phf::Map<&'static str, fn() -> tree_sitter_highlight::HighlightConfiguration> = \n{};\n",
        map.build()
    )?;

    Ok(())
}

// See https://stackoverflow.com/questions/59794375
#[derive(Debug, Deserialize)]
struct Languages {
    languages: Vec<Language>
}

#[derive(Debug, Deserialize)]
struct Language {
    name: String,
    repo: String,
    #[serde(default)]
    aliases: Vec<String>,
    command: Option<String>,
}

impl Language {
    pub fn download(&self) -> Result<Child> {
        if let Some(override_command) = &self.command {
            Command::new("sh")
                .arg("-c")
                .arg(override_command)
                .spawn()
        } else {
            Command::new("git")
                .arg("clone")
                .arg(&self.repo)
                .arg(&format!("languages/temp/{}", self.name))
                .spawn()
        }
        .map_err(Error::from)
    }

    pub fn compile(&self) {
        let path = Path::new("languages").join(&self.name).join("src");

        let has_scanner = path.join("scanner.c").exists() || path.join("scanner.cc").exists();
        let scanner_is_cpp = path.join("scanner.cc").exists();

        let mut build = cc::Build::new();

        let parser_path = path.join("parser.c");
        
        let build = build
            .include(&path)
            .flag_if_supported("-w")
            .flag_if_supported("-O1")
            .file(&parser_path);

        rerun_if_changed(&parser_path);

        if has_scanner && !scanner_is_cpp {
            let scanner_path = path.join("scanner.c");
            rerun_if_changed(&scanner_path);
            build.file(&scanner_path);
        } 
        else if scanner_is_cpp {
            let mut build = cc::Build::new();

            let scanner_path = path.join("scanner.cc");
            rerun_if_changed(&scanner_path);

            build
                .cpp(true)
                .include(&path)
                .flag_if_supported("-w")
                .flag_if_supported("-O2")
                .file(&scanner_path)
                .compile(&format!("{}-scanner", self.name));
        }

        build.compile(&format!("{}-parser", self.name));
    }

    pub fn codegen<'a>(&'a self, buffer: &mut String, map: &mut Map<&'a str>) {
        let name_ident = self.name.replace('-', "_");
        let name = &self.name;

        let highlight_path = format!("languages/{name}/queries/highlights.scm");
        let injections_path = format!("languages/{name}/queries/injections.scm");
        let locals_path = format!("languages/{name}/queries/locals.scm");

        let highlight_query = match Path::new(&highlight_path).exists() {
            false => "\"\"".to_string(),
            true => format!("include_str!(\"../{}\")", &highlight_path)
        };

        let injections_query = match Path::new(&injections_path).exists() {
            false => "\"\"".to_string(),
            true => format!("include_str!(\"../{}\")", &injections_path)
        };

        let locals_query = match Path::new(&locals_path).exists() {
            false => "\"\"".to_string(),
            true => format!("include_str!(\"../{}\")", &locals_path)
        };

        
        let generated_module = indoc::formatdoc!("
            pub mod {name_ident} {{
                use tree_sitter::Language;
                use tree_sitter_highlight::HighlightConfiguration;
            
                extern \"C\" {{
                    pub fn tree_sitter_{name_ident}() -> Language;
                }}

                pub fn config() -> HighlightConfiguration {{
                    HighlightConfiguration::new(
                        unsafe {{ tree_sitter_{name_ident}() }},
                        HIGHLIGHT_QUERY,
                        INJECTIONS_QUERY,
                        LOCALS_QUERY,
                    ).unwrap()
                }}
            
                pub const HIGHLIGHT_QUERY: &str = {highlight_query};
                pub const INJECTIONS_QUERY: &str = {injections_query};
                pub const LOCALS_QUERY: &str = {locals_query};
            
                #[cfg(test)]
                mod tests {{
                    #[test]
                    fn grammar_loading() {{
                        let mut parser = tree_sitter::Parser::new();
                        parser
                            .set_language(unsafe {{ super::tree_sitter_{name_ident}() }})
                            .expect(\"Grammar should load successfully.\");
                    }}
                }}
            }}
            
        ");
        
        let generated_config = format!("{name_ident}::config as fn() -> tree_sitter_highlight::HighlightConfiguration");

        map.entry(name, &generated_config);

        for alias in &self.aliases {
            map.entry(alias, &generated_config);
        }

        buffer.push_str(&generated_module);
    }
}

fn rerun_if_changed(path: impl AsRef<Path>) {
    println!(
        "cargo:rerun-if-changed={}",
        path.as_ref().to_str().unwrap()
    );
}