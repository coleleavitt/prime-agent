//! The feature slash-command seam: commands an installed feature registers
//! join the shared registry as session commands, and a builtin name stays
//! the builtin's. (Its own test binary: the registration is process-wide.)

use pa_types::slash_commands::{
    BuiltinSlashCommand,
    SlashCommandExecution,
    SlashCommandRegistry,
    feature_slash_commands,
    is_session_slash_command_name,
    register_feature_slash_commands,
};

#[test]
fn registered_feature_commands_are_session_commands_in_every_registry() {
    let stub = BuiltinSlashCommand {
        name: "stub-feature",
        description: "A stub feature's command",
        execution: SlashCommandExecution::Client,
        argument_hint: Some("[--flag]"),
        aliases: &["stubf"],
        takes_argument: false,
    };
    let clash = BuiltinSlashCommand {
        name: "model",
        description: "must not replace the builtin",
        ..stub
    };
    let native = SlashCommandRegistry::builtin().all().len();
    assert!(!is_session_slash_command_name("stub-feature"));
    assert!(register_feature_slash_commands(vec![stub.clone(), clash]));
    assert!(
        !register_feature_slash_commands(Vec::new()),
        "registration is once"
    );

    assert_eq!(
        feature_slash_commands(),
        &[BuiltinSlashCommand {
            execution: SlashCommandExecution::Session,
            takes_argument: true,
            ..stub
        }]
    );
    assert!(is_session_slash_command_name("stub-feature"));
    for registry in [
        SlashCommandRegistry::builtin_cached(),
        &SlashCommandRegistry::builtin(),
    ] {
        assert_eq!(registry.all().len(), native + 1);
        let resolved = registry.parse("/stubf --flag x").expect("resolves");
        assert_eq!(
            (resolved.name, resolved.args.as_str()),
            ("stub-feature", "--flag x")
        );
        assert_eq!(
            registry
                .get("stub-feature")
                .map(|command| command.execution),
            Some(SlashCommandExecution::Session)
        );
        assert_eq!(
            registry.get("model").map(|command| command.description),
            Some("Select model (opens selector UI; Tab filters by typed text)")
        );
        assert!(registry.suggestion_candidates().contains(&"stub-feature"));
    }
}
