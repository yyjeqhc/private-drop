use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use webcodex_core::plugin::validate_provider_id;

pub(crate) const PLUGIN_INIT_SDK_VERSION: &str = "0.1.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginInitOptions {
    pub(crate) directory: PathBuf,
    pub(crate) provider_id: String,
}

pub(crate) fn parse_plugin_init(args: &[String]) -> Result<PluginInitOptions, String> {
    let mut directory = None;
    let mut provider_id = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                if provider_id.is_some() {
                    return Err("--id may be specified only once".to_string());
                }
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--id requires a value".to_string())?;
                if value.is_empty() {
                    return Err("--id cannot be empty".to_string());
                }
                provider_id = Some(value.clone());
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown plugin init flag: {flag}"));
            }
            value => {
                if directory.is_some() {
                    return Err(format!("unexpected plugin init argument: {value}"));
                }
                if value.is_empty() {
                    return Err("plugin init DIRECTORY cannot be empty".to_string());
                }
                directory = Some(PathBuf::from(value));
            }
        }
        index += 1;
    }

    let directory = directory.ok_or_else(|| "plugin init requires DIRECTORY".to_string())?;
    let provider_id = match provider_id {
        Some(provider_id) => {
            validate_provider_id(&provider_id)
                .map_err(|error| format!("invalid --id provider id: {error}"))?;
            provider_id
        }
        None => derive_provider_id(&directory)?,
    };

    Ok(PluginInitOptions {
        directory,
        provider_id,
    })
}

fn derive_provider_id(directory: &Path) -> Result<String, String> {
    let basename = directory
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| {
            "destination directory basename cannot be used as a Plugin provider id; supply --id PROVIDER_ID"
                .to_string()
        })?;
    validate_provider_id(basename).map_err(|error| {
        format!(
            "destination directory basename is not a valid Plugin provider id ({error}); supply --id PROVIDER_ID"
        )
    })?;
    Ok(basename.to_string())
}

pub(crate) fn run_plugin_init(opts: PluginInitOptions) -> Result<String, String> {
    validate_provider_id(&opts.provider_id)
        .map_err(|error| format!("invalid Plugin provider id: {error}"))?;
    let files = scaffold_files(&opts.provider_id);
    prepare_destination(&opts.directory)?;

    let src = opts.directory.join("src");
    fs::create_dir(&src).map_err(|error| format!("failed to create {}: {error}", src.display()))?;

    for file in &files {
        let path = opts.directory.join(file.path);
        write_new_file(&path, file.content.as_bytes())?;
    }

    Ok(format!(
        "Created Native Tool Plugin project at {}\nProvider id: {}\nSDK: @yyjeqhc/webcodex-plugin-sdk@{}\nNext: npm install && npm run build\n",
        opts.directory.display(),
        opts.provider_id,
        PLUGIN_INIT_SDK_VERSION
    ))
}

fn prepare_destination(directory: &Path) -> Result<(), String> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "plugin init destination {} must be an ordinary directory, not a file or symlink",
                    directory.display()
                ));
            }
            ensure_directory_empty(directory)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = directory
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(|error| {
                    format!(
                        "failed to create parent directory {}: {error}",
                        parent.display()
                    )
                })?;
            }
            fs::create_dir(directory).map_err(|error| {
                format!(
                    "failed to create plugin init destination {}: {error}",
                    directory.display()
                )
            })
        }
        Err(error) => Err(format!(
            "failed to inspect plugin init destination {}: {error}",
            directory.display()
        )),
    }
}

fn ensure_directory_empty(directory: &Path) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?;
    match entries.next() {
        None => Ok(()),
        Some(Ok(_)) => Err(format!(
            "plugin init destination {} is not empty; refusing to overwrite existing data",
            directory.display()
        )),
        Some(Err(error)) => Err(format!(
            "failed to inspect contents of {}: {error}",
            directory.display()
        )),
    }
}

fn write_new_file(path: &Path, content: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("refusing to overwrite {}: {error}", path.display()))?;
    file.write_all(content)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

struct ScaffoldFile {
    path: &'static str,
    content: String,
}

fn scaffold_files(provider_id: &str) -> Vec<ScaffoldFile> {
    vec![
        ScaffoldFile {
            path: ".gitignore",
            content: "node_modules/\ndist/\n".to_string(),
        },
        ScaffoldFile {
            path: "package.json",
            content: format!(
                "{{\n  \"private\": true,\n  \"type\": \"module\",\n  \"scripts\": {{\n    \"typecheck\": \"tsc -p tsconfig.json --noEmit\",\n    \"build\": \"tsc -p tsconfig.json\"\n  }},\n  \"engines\": {{\n    \"node\": \">=18\"\n  }},\n  \"dependencies\": {{\n    \"@yyjeqhc/webcodex-plugin-sdk\": \"{}\"\n  }},\n  \"devDependencies\": {{\n    \"@types/node\": \"^18.19.0\",\n    \"typescript\": \"^5.4.0\"\n  }}\n}}\n",
                PLUGIN_INIT_SDK_VERSION
            ),
        },
        ScaffoldFile {
            path: "tsconfig.json",
            content: "{\n  \"compilerOptions\": {\n    \"target\": \"ES2022\",\n    \"module\": \"NodeNext\",\n    \"moduleResolution\": \"NodeNext\",\n    \"lib\": [\"ES2022\"],\n    \"types\": [\"node\"],\n    \"strict\": true,\n    \"exactOptionalPropertyTypes\": true,\n    \"noUncheckedIndexedAccess\": true,\n    \"verbatimModuleSyntax\": true,\n    \"rootDir\": \"src\",\n    \"outDir\": \"dist\",\n    \"declaration\": false,\n    \"sourceMap\": false,\n    \"skipLibCheck\": true\n  },\n  \"include\": [\"src/**/*.ts\"]\n}\n"
                .to_string(),
        },
        ScaffoldFile {
            path: "src/plugin.ts",
            content: "import {\n  definePlugin,\n  defineTool,\n  runPlugin,\n  schema,\n  textResult,\n} from \"@yyjeqhc/webcodex-plugin-sdk\";\n\nconst echo = defineTool({\n  name: \"echo\",\n  description: \"Echo one string\",\n  inputSchema: schema.object({\n    text: schema.string({ minLength: 1, maxLength: 4096 }),\n  }),\n  outputSchema: schema.object({\n    text: schema.string({ maxLength: 4096 }),\n  }),\n  annotations: {\n    readOnlyHint: true,\n    destructiveHint: false,\n    idempotentHint: true,\n    openWorldHint: false,\n  },\n  async execute({ text }) {\n    return textResult(text, { text });\n  },\n});\n\nrunPlugin(definePlugin({ tools: [echo] }));\n"
                .to_string(),
        },
        ScaffoldFile {
            path: "README.md",
            content: format!(
                "# {provider_id}\n\nThis project is a TypeScript Native Tool Plugin scaffold generated by `webcodex plugin init`. It uses the published `@yyjeqhc/webcodex-plugin-sdk@{PLUGIN_INIT_SDK_VERSION}` package and does not depend on a WebCodex source checkout. Node.js 18 or newer is required to run the compiled Plugin.\n\n## Install and build\n\n```bash\nnpm install\nnpm run build\n```\n\n`npm run typecheck` is also available for a no-output type check. `plugin init` itself does not install dependencies or execute generated code.\n\n## Configure one Runner manually\n\nCopy and edit this provider entry in the target Runner's `runner.toml`. Replace the absolute path placeholder with the real path to this project's compiled `dist/plugin.js`.\n\n```toml\n[[plugins.providers]]\nid = \"{provider_id}\"\nname = \"{provider_id}\"\ncommand = \"node\"\nargs = [\"/absolute/path/to/PLUGIN_DIRECTORY/dist/plugin.js\"]\ntimeout_secs = 30\n```\n\nWebCodex does not edit Runner configuration as part of `plugin init`.\n\n## Author loop\n\n```text\nwebcodex plugin check --runner <runner> --plugin {provider_id}\nwebcodex plugin reload --runner <runner>\nwebcodex plugin list --runner <runner> --plugin {provider_id}\nwebcodex plugin describe --runner <runner> --plugin {provider_id} --tool echo\n```\n\nInvocation remains on the canonical existing `plugin_tool describe -> call` path; there is intentionally no `webcodex plugin call` command.\n"
            ),
        },
    ]
}
