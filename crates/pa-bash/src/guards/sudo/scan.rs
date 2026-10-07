//! The sudo walk: every command word of the text (and of the payloads it
//! runs: `sh -c`, `eval`, substitutions, process substitutions, heredocs fed
//! to a runner, alias bodies, `env -S` strings, `xargs`/`find -exec`
//! operands) is judged for whether it runs sudo or doas.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::lexer::{
    apply_heredocs, chars_from, is_digit, matching_paren, strip_quotes, tokenize, Kind, Word,
};
use super::names::{basename, word_names_sudo};
use super::tables::{
    exec_launcher_flags, is_payload_runner, launcher_operand_options, wrapper_options, KEYWORDS,
    LOOKUP_COMMANDS, MAX_PAYLOAD_DEPTH, SHADOWPROOF_BUILTINS, SHELL_RUNNERS, WRAPPERS,
};

/// Why the text would escalate. Its display is the reason phrase the refusal
/// message embeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Violation {
    /// A command word that is (or can expand to) sudo/doas.
    Escalates { name: String },
    /// A `hash -p` entry makes a word run sudo/doas.
    HashEntry { word: String, target: String },
    /// A command word built from expansion while the text names sudo/doas.
    UnknownCommandWord,
    /// A `hash -p` registration built from expansion.
    UnreadableHashEntry,
    /// A shell reads its script from a process substitution naming sudo/doas.
    ProcessSubstitutionScript,
    /// The payloads nest deeper than the scan follows.
    TooDeep,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::Escalates { name } => {
                write!(f, "{name} would run this command as root or another user")
            }
            Violation::HashEntry { word, target } => write!(
                f,
                "a `hash -p` entry makes {word} run {target}, which would run this command as \
                 root or another user"
            ),
            Violation::UnknownCommandWord => f.write_str(
                "the command position expands to an unknown program while the text invokes \
                 sudo/doas",
            ),
            Violation::UnreadableHashEntry => f.write_str(
                "a `hash -p` registration builds the command it runs from expansion, so that \
                 entry cannot be resolved",
            ),
            Violation::ProcessSubstitutionScript => f.write_str(
                "the shell reads its script from a process substitution whose text invokes \
                 sudo/doas",
            ),
            Violation::TooDeep => {
                f.write_str("the payload nests deeper than the sudo scan can follow")
            }
        }
    }
}

/// `hash -p pathname name` registrations: name -> the file it runs.
type HashNames = BTreeMap<String, String>;

/// Judge `text` (already line-joined) at payload `depth`; `inherited` is set
/// when an enclosing text names sudo/doas.
pub(super) fn scan_text(text: &str, depth: usize, inherited: bool) -> Option<Violation> {
    let mut words = tokenize(text);
    apply_heredocs(text, &mut words);
    let inner = inherited || mentions_sudo(&words);
    // `hash -p pathname name` makes a later `name` run `pathname`: those names
    // scan as the command they run, and an entry the guard cannot read is
    // refused, because the command it hides cannot be resolved at all.
    let (hash_names, unreadable) = hash_registered_command_names(&words);
    if unreadable {
        return Some(Violation::UnreadableHashEntry);
    }
    for word in &words {
        if word.is_data || !word.has_expansion {
            continue;
        }
        if let Some(violation) = scan_expansion(&word.value, depth, inner) {
            return Some(violation);
        }
    }
    let walk = Walk {
        words: &words,
        depth,
        inherited: inner,
    };
    // The words the walk reaches as command words, so the heredoc gate below
    // uses the same judgment as the refusals.
    let mut command_words = BTreeSet::new();
    for (index, word) in words.iter().enumerate() {
        if word.is_data || !word.starts_command {
            continue;
        }
        if let Some(violation) = walk.segment(index, &mut command_words, Some(&hash_names)) {
            return Some(violation);
        }
    }
    // A heredoc body that the same text feeds to a runner is a script.
    let runner_alias = alias_body_names_runner(&words, &command_words);
    let feeds_runner = runner_alias
        || command_words.iter().any(|&position| {
            is_payload_runner(basename(registered_command(
                &words[position].value,
                Some(&hash_names),
            )))
        });
    if depth < MAX_PAYLOAD_DEPTH && feeds_runner {
        for word in &words {
            let Some(body) = word.heredoc_body.as_deref().filter(|body| !body.is_empty()) else {
                continue;
            };
            if word.is_data {
                continue;
            }
            if let Some(violation) = scan_text(body, depth + 1, inner) {
                return Some(violation);
            }
        }
    }
    if runner_alias && depth < MAX_PAYLOAD_DEPTH {
        // An alias whose body names a runner can still read a redirect as its
        // script, so judge this text's redirects the way a runner's own are.
        let all: Vec<usize> = (0..words.len()).collect();
        return walk.script_source(&all);
    }
    None
}

/// Scan the `$(...)`, backtick and process-substitution spans inside a word:
/// they run as commands of their own.
fn scan_expansion(value: &str, depth: usize, inherited: bool) -> Option<Violation> {
    if depth >= MAX_PAYLOAD_DEPTH {
        return Some(Violation::TooDeep);
    }
    expansion_spans(value)
        .iter()
        .find_map(|span| scan_text(span, depth + 1, inherited))
}

/// `$(...)`, `<(...)`/`>(...)` and backtick spans of a word, as command text
/// (an unterminated span runs to the end of the word).
fn expansion_spans(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    let length = chars.len();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < length {
        let ch = chars[index];
        let opens = chars.get(index + 1) == Some(&'(') && matches!(ch, '$' | '<' | '>');
        if opens {
            let close = matching_paren(&chars, index + 1, length);
            if chars.get(close) == Some(&')') {
                spans.push(chars[index + 2..close].iter().collect());
                index = close + 1;
            } else {
                spans.push(chars[(index + 2).min(length)..].iter().collect());
                index = length;
            }
            continue;
        }
        if ch == '`' {
            let Some(end) = (index + 1..length).find(|&i| chars[i] == '`') else {
                break;
            };
            spans.push(chars[index + 1..end].iter().collect());
            index = end + 1;
            continue;
        }
        index += 1;
    }
    spans
}

/// True when any word (or assignment value) names sudo/doas, obfuscations
/// included.
fn mentions_sudo(words: &[Word]) -> bool {
    words.iter().filter(|word| !word.is_operator()).any(|word| {
        word_names_sudo(&word.value)
            || (word.is_assignment
                && word
                    .value
                    .split_once('=')
                    .is_some_and(|(_, value)| word_names_sudo(value.trim_start_matches('+'))))
    })
}

/// Indices of the words from `start` up to the segment's operator.
fn segment_tail(words: &[Word], start: usize) -> Vec<usize> {
    (start..words.len())
        .take_while(|&index| !words[index].is_operator())
        .collect()
}

/// The file a `hash -p` registration makes this word run, else the word
/// (the builtins the hash table cannot shadow keep their own meaning).
fn registered_command<'a>(value: &'a str, hash_names: Option<&'a HashNames>) -> &'a str {
    let Some(names) = hash_names.filter(|names| !names.is_empty()) else {
        return value;
    };
    match names.get(value) {
        Some(target) if !SHADOWPROOF_BUILTINS.contains(&value) => target,
        Some(_) | None => value,
    }
}

/// `hash -p pathname name...` entries, and whether one is unreadable (its
/// target or a name is built from expansion, or a name is a pattern). `hash`
/// without `-p` only reads or clears the table.
fn hash_registered_command_names(words: &[Word]) -> (HashNames, bool) {
    let mut aliased = HashNames::new();
    let mut unreadable = false;
    for (index, word) in words.iter().enumerate() {
        if word.value != "hash" {
            continue;
        }
        let mut has_pathname_option = false;
        let mut operands: Vec<String> = Vec::new();
        for candidate in segment_tail(words, index + 1) {
            let candidate = &words[candidate];
            let token = &candidate.value;
            if candidate.is_data || candidate.is_redirect() {
                break;
            }
            if candidate.is_flag() && !token.starts_with("--") {
                if let Some(p) = token.char_indices().skip(1).find(|(_, ch)| *ch == 'p') {
                    has_pathname_option = true;
                    // `hash -p/path name` glues the pathname to the flag.
                    let glued = &token[p.0 + 1..];
                    if !glued.is_empty() {
                        operands.push(glued.to_string());
                    }
                }
                continue;
            }
            if token.starts_with("--") {
                continue;
            }
            if !has_pathname_option {
                break; // `hash name`, `hash -d name`: no entry
            }
            operands.push(token.clone());
        }
        if !has_pathname_option || operands.len() < 2 {
            continue;
        }
        let (pathname, names) = (&operands[0], &operands[1..]);
        let expands = |text: &str| text.contains(['$', '`']);
        if expands(pathname) || names.iter().any(|name| expands(name)) {
            unreadable = true;
            continue;
        }
        if names
            .iter()
            .any(|name| name.contains(['*', '?', '[', ']', '{', '}']))
        {
            // A pattern name registers whatever it expands to.
            unreadable = true;
            continue;
        }
        for name in names {
            aliased.insert(name.clone(), pathname.clone());
        }
    }
    (aliased, unreadable)
}

/// Value half of an `alias NAME=BODY` operand.
fn alias_body(word: &Word) -> Option<String> {
    if word.is_data || word.is_redirect() {
        return None;
    }
    let (_, body) = word.value.split_once('=')?;
    let body = body.trim_start_matches('+');
    (!body.is_empty()).then(|| body.to_string())
}

/// True when a runner is a command word of `body`, wrapper chains and the
/// aliases the body itself defines included.
pub(super) fn body_reaches_runner(body: &str, depth: usize) -> bool {
    let mut words = tokenize(body);
    apply_heredocs(body, &mut words);
    let (hash_names, _) = hash_registered_command_names(&words);
    let walk = Walk {
        words: &words,
        depth: 0,
        inherited: false,
    };
    let mut reached = BTreeSet::new();
    for (index, word) in words.iter().enumerate() {
        if word.is_data || !word.starts_command {
            continue;
        }
        // Only the command words reached matter here, not the verdict.
        let _ = walk.segment(index, &mut reached, Some(&hash_names));
    }
    if reached.iter().any(|&index| {
        is_payload_runner(basename(registered_command(
            &words[index].value,
            Some(&hash_names),
        )))
    }) {
        return true;
    }
    if depth >= MAX_PAYLOAD_DEPTH {
        // Too deep to resolve: the chain could still reach a runner.
        return true;
    }
    // `alias a='alias b=sh'` runs the payload through `b`.
    reached
        .iter()
        .filter(|&&index| basename(&words[index].value) == "alias")
        .any(|&index| {
            segment_tail(&words, index + 1)
                .into_iter()
                .any(|candidate| {
                    alias_body(&words[candidate])
                        .is_some_and(|nested| body_reaches_runner(&nested, depth + 1))
                })
        })
}

/// True when an alias defined in this text runs a payload runner.
fn alias_body_names_runner(words: &[Word], command_words: &BTreeSet<usize>) -> bool {
    command_words
        .iter()
        .filter(|&&index| basename(&words[index].value) == "alias")
        .any(|&index| {
            segment_tail(words, index + 1).into_iter().any(|candidate| {
                alias_body(&words[candidate]).is_some_and(|body| body_reaches_runner(&body, 0))
            })
        })
}

/// Match a flag word against an option set: the option and its glued operand.
/// A short bundle (`env -vu NAME`, `xargs -rn 2`) is scanned for a
/// value-taking letter: letters before it are plain flags, after it the
/// glued operand.
fn split_option(value: &str, options: &[&str], letters: &str) -> (Option<String>, Option<String>) {
    if options.contains(&value) {
        return (Some(value.to_string()), None);
    }
    if value.starts_with("--") {
        if let Some((option, glued)) = value.split_once('=') {
            if options.contains(&option) {
                return (Some(option.to_string()), Some(glued.to_string()));
            }
            return (None, None);
        }
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() > 2 && chars[0] == '-' && chars[1] != '-' {
        let Some(offset) = chars[1..].iter().position(|ch| letters.contains(*ch)) else {
            return (None, None);
        };
        let glued: String = chars[2 + offset..].iter().collect();
        return (
            Some(format!("-{}", chars[1 + offset])),
            (!glued.is_empty()).then_some(glued),
        );
    }
    (None, None)
}

/// A GNU `timeout` duration (a number with an optional unit suffix), the
/// spelling that must not read as the command word.
fn is_duration(value: &str) -> bool {
    let digits = match value.chars().last() {
        Some(last) if last.is_alphabetic() => &value[..value.len() - last.len_utf8()],
        Some(_) | None => value,
    };
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (digits, None),
    };
    let all_digits = |text: &str| text.chars().all(is_digit);
    match fraction {
        None => !whole.is_empty() && all_digits(whole),
        Some(fraction) if whole.is_empty() => !fraction.is_empty() && all_digits(fraction),
        Some(fraction) => all_digits(whole) && all_digits(fraction),
    }
}

/// `-c`, a bundled flag containing `c` (`-lc`), or `--command`.
fn is_command_flag(value: &str) -> bool {
    if value == "--command" {
        return true;
    }
    let mut chars = value.chars();
    chars.next() == Some('-') && {
        let rest = chars.as_str();
        !rest.is_empty() && rest.contains('c') && rest.chars().all(char::is_alphabetic)
    }
}

/// A payload folded onto the flag word itself (`-c$'sudo id'` -> `-csudo id`);
/// only short bundles fold one.
fn glued_payload(value: &str) -> Option<&str> {
    if !value.starts_with('-') || value.starts_with("--") || !value[1..].contains('c') {
        return None;
    }
    let after = &value[value.find('c')? + 1..];
    (!after.is_empty()).then_some(after)
}

/// Inner command text of a `<(cmd)` process substitution.
fn process_substitution_body(value: &str) -> Option<String> {
    if !value.starts_with("<(") {
        return None;
    }
    let chars: Vec<char> = value.chars().collect();
    let mut close = matching_paren(&chars, 1, chars.len());
    if chars.get(close) != Some(&')') {
        close = chars.len(); // unterminated: the remainder runs
    }
    Some(chars[2..close.max(2)].iter().collect())
}

/// Words that end an option walk (a wrapper's operand can be none of these).
fn ends_segment(word: &Word) -> bool {
    word.is_operator() || word.is_redirect() || word.is_data || word.is_operand
}

/// The segment walk over one tokenized text.
struct Walk<'w> {
    words: &'w [Word],
    depth: usize,
    /// The enclosing text names sudo/doas.
    inherited: bool,
}

impl Walk<'_> {
    /// Walk one command segment to its command word and judge that word,
    /// recording every word reached as a command word in `reached`.
    fn segment(
        &self,
        mut start: usize,
        reached: &mut BTreeSet<usize>,
        hash_names: Option<&HashNames>,
    ) -> Option<Violation> {
        let words = self.words;
        while start < words.len() {
            let word = &words[start];
            if word.is_operator() {
                return None;
            }
            if word.is_data || word.is_redirect() || word.is_operand || word.is_assignment {
                start += 1;
                continue;
            }
            if let Some(next) = skip_syntax(words, start) {
                start = next;
                continue;
            }
            // A `hash -p` registration makes the word run a file whatever it
            // looks like; the entry can be inert by run time (`hash -r`), so
            // the word's own spelling is judged too.
            let registered = registered_command(&word.value, hash_names);
            if registered != word.value {
                if let Some(violation) = self.segment(start, reached, None) {
                    return Some(violation);
                }
            }
            let name = basename(registered);
            if LOOKUP_COMMANDS.contains(&name) {
                return None; // `which sudo`: operands are just names
            }
            if name == "command" {
                // `command` is a lookup with -v/-V and otherwise still runs
                // the next word.
                start += 1;
                let mut lookup = false;
                while start < words.len() && words[start].is_flag() {
                    lookup |= words[start].value.contains(['v', 'V']);
                    start += 1;
                }
                if lookup {
                    return None;
                }
                continue;
            }
            if WRAPPERS.contains(&name) {
                match self.skip_wrapper_operands(start + 1, name) {
                    Ok(next) => start = next,
                    Err(violation) => return Some(violation),
                }
                continue;
            }
            reached.insert(start);
            if word_names_sudo(&word.value) {
                return Some(Violation::Escalates {
                    name: basename(&word.value).to_string(),
                });
            }
            if word_names_sudo(registered) {
                return Some(Violation::HashEntry {
                    word: word.value.clone(),
                    target: registered.to_string(),
                });
            }
            if name == "alias" {
                return self.alias_bodies(start + 1);
            }
            if let Some(flags) = exec_launcher_flags(name) {
                return self.find_execs(start + 1, reached, flags, hash_names);
            }
            if word.has_expansion {
                if self.inherited || mentions_sudo(words) {
                    return Some(Violation::UnknownCommandWord);
                }
                return None;
            }
            return self.interpreter(start, reached, hash_names);
        }
        None
    }

    /// Index after a wrapper's own operands, or the violation an `env -S`
    /// split string carries.
    fn skip_wrapper_operands(&self, mut index: usize, wrapper: &str) -> Result<usize, Violation> {
        let words = self.words;
        let table = wrapper_options(wrapper);
        let mut leading = table.leading_operands;
        while index < words.len() {
            let word = &words[index];
            if word.is_operator() || word.is_redirect() || word.is_data {
                break;
            }
            if word.is_assignment {
                index += 1;
                continue;
            }
            if word.is_flag() {
                let (Some(option), glued) = split_option(&word.value, table.options, table.letters)
                else {
                    index += 1;
                    continue;
                };
                // env -S/--split-string takes a whole command line: its
                // operand is text a shell runs.
                let split_string =
                    wrapper == "env" && (option == "-S" || option == "--split-string");
                let Some(glued) = glued else {
                    let operand = index + 1;
                    if split_string && operand < words.len() && !ends_segment(&words[operand]) {
                        if let Some(violation) =
                            scan_text(&words[operand].value, self.depth + 1, self.inherited)
                        {
                            return Err(violation);
                        }
                    }
                    index += 2;
                    continue;
                };
                if split_string {
                    if let Some(violation) = scan_text(&glued, self.depth + 1, self.inherited) {
                        return Err(violation);
                    }
                }
                index += 1;
                continue;
            }
            if (wrapper == "nice" || wrapper == "timeout") && is_duration(&word.value) {
                index += 1;
                continue;
            }
            if leading > 0 {
                // A positional operand (chroot's NEWROOT, faketime's timestamp).
                leading -= 1;
                index += 1;
                continue;
            }
            break;
        }
        Ok(index)
    }

    /// Judge the payloads a runner executes: `sh -c`, `eval`, xargs operands,
    /// heredocs and here-strings.
    fn interpreter(
        &self,
        index: usize,
        reached: &mut BTreeSet<usize>,
        hash_names: Option<&HashNames>,
    ) -> Option<Violation> {
        let words = self.words;
        let name = basename(registered_command(&words[index].value, hash_names));
        let launcher = launcher_operand_options(name);
        if !is_payload_runner(name) && launcher.is_none() {
            return None;
        }
        if self.depth >= MAX_PAYLOAD_DEPTH {
            return Some(Violation::TooDeep);
        }
        let following = segment_tail(words, index + 1);
        if let Some((options, letters)) = launcher {
            return self.launcher_operands(&following, reached, options, letters, hash_names);
        }
        if SHELL_RUNNERS.contains(&name) {
            for (offset, &candidate) in following.iter().enumerate() {
                let word = &words[candidate];
                if word.is_data {
                    break;
                }
                if word.is_redirect() {
                    // `bash -c >/tmp/out 'sudo id'` still runs the payload.
                    continue;
                }
                let command_flag = is_command_flag(&word.value);
                if command_flag {
                    let payload = following[offset + 1..]
                        .iter()
                        .find(|&&position| !words[position].is_redirect());
                    if let Some(&payload) = payload {
                        let text = strip_quotes(&words[payload].value);
                        if let Some(violation) = scan_text(text, self.depth + 1, self.inherited) {
                            return Some(violation);
                        }
                    }
                }
                if let Some(glued) = glued_payload(&word.value) {
                    if let Some(violation) = scan_text(glued, self.depth + 1, self.inherited) {
                        return Some(violation);
                    }
                    break;
                }
                if command_flag {
                    break;
                }
            }
        }
        if name == "eval" {
            let joined = following
                .iter()
                .filter(|&&position| !words[position].is_data)
                .map(|&position| words[position].value.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if !joined.is_empty() {
                if let Some(violation) = scan_text(&joined, self.depth + 1, self.inherited) {
                    return Some(violation);
                }
            }
        }
        if let Some(violation) = self.script_source(&following) {
            return Some(violation);
        }
        // A heredoc body owned by a runner is a script, not data.
        following.iter().find_map(|&candidate| {
            words[candidate]
                .heredoc_body
                .as_deref()
                .filter(|body| !body.is_empty())
                .and_then(|body| scan_text(body, self.depth + 1, self.inherited))
        })
    }

    /// `xargs`/`parallel` run their first non-flag word; option operands are
    /// judged as commands too (BSD and GNU disagree on which are optional).
    fn launcher_operands(
        &self,
        following: &[usize],
        reached: &mut BTreeSet<usize>,
        options: &[&str],
        letters: &str,
        hash_names: Option<&HashNames>,
    ) -> Option<Violation> {
        let words = self.words;
        let mut position = 0;
        while position < following.len() {
            let word = &words[following[position]];
            if word.is_data || word.is_redirect() {
                return None;
            }
            if !word.is_flag() {
                return self.segment(following[position], reached, hash_names);
            }
            let (matched, glued) = split_option(&word.value, options, letters);
            if matched.is_some() && glued.is_none() && position + 1 < following.len() {
                let operand = &words[following[position + 1]];
                if !operand.is_data && !operand.is_redirect() {
                    if let Some(violation) =
                        self.segment(following[position + 1], reached, hash_names)
                    {
                        return Some(violation);
                    }
                }
                position += 2;
                continue;
            }
            position += 1;
        }
        None
    }

    /// Judge a runner's script given as a redirect: `<<<` text or the output
    /// of a `<(cmd)` process substitution.
    fn script_source(&self, following: &[usize]) -> Option<Violation> {
        let words = self.words;
        for &candidate in following {
            let word = &words[candidate];
            if word.heredoc == Some("<<<") {
                // The payload can be glued to the operator (`bash<<<'sudo id'`)
                // or be the next word (`bash <<< 'sudo id'`).
                let glued = chars_from(&word.value, 3);
                let mut sources = Vec::new();
                if !glued.is_empty() {
                    sources.push(glued);
                }
                if let Some(operand) = words.get(candidate + 1).filter(|operand| !operand.is_data) {
                    sources.push(operand.value.clone());
                }
                for source in sources {
                    if let Some(violation) =
                        scan_text(strip_quotes(&source), self.depth + 1, self.inherited)
                    {
                        return Some(violation);
                    }
                }
                continue;
            }
            if word.is_data {
                continue;
            }
            if let Some(body) = process_substitution_body(&word.value) {
                if !body.is_empty() && (self.inherited || mentions_sudo(&tokenize(&body))) {
                    return Some(Violation::ProcessSubstitutionScript);
                }
            }
        }
        None
    }

    /// An alias body is text that a later use of the alias runs.
    fn alias_bodies(&self, start: usize) -> Option<Violation> {
        segment_tail(self.words, start)
            .into_iter()
            .find_map(|candidate| {
                alias_body(&self.words[candidate])
                    .and_then(|body| scan_text(&body, self.depth + 1, self.inherited))
            })
    }

    /// `find -exec cmd` (and `fd -x cmd`) runs cmd, so its operand is judged
    /// as a command.
    fn find_execs(
        &self,
        start: usize,
        reached: &mut BTreeSet<usize>,
        flags: &[&str],
        hash_names: Option<&HashNames>,
    ) -> Option<Violation> {
        let words = self.words;
        let tail = segment_tail(words, start);
        for (offset, &candidate) in tail.iter().enumerate() {
            if !flags.contains(&words[candidate].value.as_str()) || offset + 1 >= tail.len() {
                continue;
            }
            let operand = &words[tail[offset + 1]];
            if operand.is_data || operand.is_redirect() {
                continue;
            }
            if let Some(violation) = self.segment(tail[offset + 1], reached, hash_names) {
                return Some(violation);
            }
        }
        None
    }
}

/// When the word at `start` is compound-command syntax rather than a program
/// (a keyword, a group brace, a loop or `case` header, `time` and its flags,
/// a `coproc` name), the index of the next word the walk reads.
fn skip_syntax(words: &[Word], mut start: usize) -> Option<usize> {
    let word = &words[start];
    match word.value.as_str() {
        "for" | "select" => {
            // The loop variable and `in` list are names.
            start += 1;
            while start < words.len()
                && !words[start].is_operator()
                && !matches!(words[start].value.as_str(), "do" | "done")
            {
                start += 1;
            }
        }
        "case" => start = skip_case_header(words, start + 1),
        "time" => {
            // `time` is a keyword: its own flags are not the command word.
            start += 1;
            while start < words.len() && words[start].is_flag() {
                start += 1;
            }
        }
        "coproc" => {
            // `coproc [NAME] command`: the first plain word is the name only
            // when the next word starts a compound command.
            start += 1;
            if start + 1 < words.len()
                && words[start].kind == Kind::Word
                && (KEYWORDS.contains(&words[start + 1].value.as_str())
                    || words[start + 1].kind == Kind::Group)
            {
                start += 1;
            }
        }
        value if word.kind == Kind::Group || KEYWORDS.contains(&value) => start += 1,
        _ => return None,
    }
    Some(start)
}

/// Index of the `)` that closes the first `case` label list: subject and
/// labels are names.
fn skip_case_header(words: &[Word], mut start: usize) -> usize {
    while start < words.len() {
        let word = &words[start];
        if word.is_operator() {
            if word.value == ")" || (word.value != "(" && word.value != "|") {
                break;
            }
        } else if word.value == "esac" {
            break;
        }
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_chain_at_depth_limit_reaches_a_runner() {
        assert!(body_reaches_runner("alias p=sh", MAX_PAYLOAD_DEPTH));
    }

    #[test]
    fn durations_and_split_options() {
        assert!(is_duration("0.1"));
        assert!(is_duration("5s"));
        assert!(is_duration(".5"));
        assert!(!is_duration("now"));
        assert_eq!(
            split_option("-vu", &["-u"], "uC"),
            (Some("-u".to_string()), None)
        );
        assert_eq!(
            split_option("-iS'sudo id'", &["-S"], "uCSaP"),
            (Some("-S".to_string()), Some("'sudo id'".to_string()))
        );
    }
}
