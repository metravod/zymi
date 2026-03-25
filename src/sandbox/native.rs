use std::path::Path;

use super::SandboxedCommand;

/// Passthrough: no sandboxing, direct `sh -c <command>`.
pub fn wrap_command(command: &str) -> SandboxedCommand {
    SandboxedCommand {
        program: "sh".into(),
        args: vec!["-c".into(), command.into()],
        env: vec![],
    }
}

/// Passthrough: direct `<interpreter> <script_path>`.
pub fn wrap_script(interpreter: &str, script_path: &Path) -> SandboxedCommand {
    SandboxedCommand {
        program: interpreter.into(),
        args: vec![script_path.to_string_lossy().into()],
        env: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_command_passthrough() {
        let cmd = wrap_command("echo hello");
        assert_eq!(cmd.program, "sh");
        assert_eq!(cmd.args, vec!["-c", "echo hello"]);
        assert!(cmd.env.is_empty());
    }

    #[test]
    fn wrap_script_passthrough() {
        let cmd = wrap_script("python3", Path::new("/tmp/test.py"));
        assert_eq!(cmd.program, "python3");
        assert_eq!(cmd.args, vec!["/tmp/test.py"]);
        assert!(cmd.env.is_empty());
    }
}
