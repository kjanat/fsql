//! Launch the local TypeScript fsql language server from Zed.
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/kjanat/fsql/master/media/fsql-icon.svg",
    html_favicon_url = "https://raw.githubusercontent.com/kjanat/fsql/master/media/fsql-icon.svg"
)]

use zed_extension_api::{self as zed, settings::LspSettings};

struct Fsql;

impl zed::Extension for Fsql {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let binary = LspSettings::for_worktree(id.as_ref(), worktree)?.binary;
        let command = binary
            .as_ref()
            .and_then(|binary| binary.path.clone())
            .or_else(|| worktree.which("fsql-lsp"))
            .ok_or("Install fsql-lsp on PATH or set lsp.fsql.binary.path and arguments in Zed settings")?;
        Ok(zed::Command {
            command,
            args: binary
                .and_then(|binary| binary.arguments)
                .unwrap_or_else(|| vec!["--stdio".into()]),
            env: worktree.shell_env(),
        })
    }
}

zed::register_extension!(Fsql);
