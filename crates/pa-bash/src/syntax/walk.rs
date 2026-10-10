//! Visiting every word and redirect of a parsed tree.

use super::ast::{AndOr, AssignValue, Command, List, Part, Redirect, Word};

/// Every redirect of `list`, including the ones inside substitutions.
pub(crate) fn visit_redirects_mut(list: &mut List, visit: &mut dyn FnMut(&mut Redirect)) {
    for item in &mut list.items {
        and_or_redirects(item, visit);
    }
}

fn and_or_redirects(item: &mut AndOr, visit: &mut dyn FnMut(&mut Redirect)) {
    let pipelines = std::iter::once(&mut item.first).chain(item.rest.iter_mut().map(|(_, p)| p));
    for pipeline in pipelines {
        for command in &mut pipeline.commands {
            command_redirects(command, visit);
        }
    }
}

fn redirects_of(redirects: &mut [Redirect], visit: &mut dyn FnMut(&mut Redirect)) {
    for redirect in redirects {
        visit(redirect);
        word_redirects(&mut redirect.target, visit);
        if let Some(heredoc) = redirect.heredoc.as_mut() {
            word_redirects(&mut heredoc.body, visit);
        }
    }
}

fn command_redirects(command: &mut Command, visit: &mut dyn FnMut(&mut Redirect)) {
    match command {
        Command::Simple(simple) => {
            for assignment in &mut simple.assignments {
                match &mut assignment.value {
                    AssignValue::Scalar(word) => word_redirects(word, visit),
                    AssignValue::Array(words) => {
                        for word in words {
                            word_redirects(word, visit);
                        }
                    }
                }
            }
            for word in &mut simple.words {
                word_redirects(word, visit);
            }
            redirects_of(&mut simple.redirects, visit);
        }
        Command::Subshell(list, redirects) | Command::Group(list, redirects) => {
            visit_redirects_mut(list, visit);
            redirects_of(redirects, visit);
        }
        Command::Branches(lists, redirects) => {
            for list in lists {
                visit_redirects_mut(list, visit);
            }
            redirects_of(redirects, visit);
        }
        Command::For {
            words,
            body,
            redirects,
            ..
        } => {
            for word in words.iter_mut().flatten() {
                word_redirects(word, visit);
            }
            visit_redirects_mut(body, visit);
            redirects_of(redirects, visit);
        }
        Command::Case {
            word,
            arms,
            redirects,
        } => {
            word_redirects(word, visit);
            for (patterns, body) in arms {
                for pattern in patterns {
                    word_redirects(pattern, visit);
                }
                visit_redirects_mut(body, visit);
            }
            redirects_of(redirects, visit);
        }
        Command::Conditional(words) => {
            for word in words {
                word_redirects(word, visit);
            }
        }
        Command::Arithmetic(word) => word_redirects(word, visit),
        Command::Function { body, .. } => command_redirects(body, visit),
        Command::Unparsed(_) => {}
    }
}

fn word_redirects(word: &mut Word, visit: &mut dyn FnMut(&mut Redirect)) {
    for part in &mut word.parts {
        match part {
            Part::Literal(..) => {}
            Part::Parameter { operator, .. } => {
                if let Some(operator) = operator {
                    word_redirects(operator, visit);
                }
            }
            Part::Command { body, .. } | Part::Process { body, .. } => {
                visit_redirects_mut(body, visit);
            }
            Part::Arithmetic(expression) => word_redirects(expression, visit),
        }
    }
}

/// Every word of `list` at this level (command words, assignment values,
/// redirect targets, here-document bodies, loop and case words), without
/// descending into the words' own substitutions.
pub(crate) fn visit_words(list: &List, visit: &mut dyn FnMut(&Word)) {
    for item in &list.items {
        for pipeline in item.pipelines() {
            for command in &pipeline.commands {
                command_words(command, visit);
            }
        }
    }
}

fn redirect_words(redirects: &[Redirect], visit: &mut dyn FnMut(&Word)) {
    for redirect in redirects {
        visit(&redirect.target);
        if let Some(heredoc) = &redirect.heredoc {
            visit(&heredoc.body);
        }
    }
}

fn command_words(command: &Command, visit: &mut dyn FnMut(&Word)) {
    match command {
        Command::Simple(simple) => {
            for assignment in &simple.assignments {
                match &assignment.value {
                    AssignValue::Scalar(word) => visit(word),
                    AssignValue::Array(words) => words.iter().for_each(&mut *visit),
                }
            }
            simple.words.iter().for_each(&mut *visit);
            redirect_words(&simple.redirects, visit);
        }
        Command::Subshell(list, redirects) | Command::Group(list, redirects) => {
            visit_words(list, visit);
            redirect_words(redirects, visit);
        }
        Command::Branches(lists, redirects) => {
            for list in lists {
                visit_words(list, visit);
            }
            redirect_words(redirects, visit);
        }
        Command::For {
            words,
            body,
            redirects,
            ..
        } => {
            words.iter().flatten().for_each(&mut *visit);
            visit_words(body, visit);
            redirect_words(redirects, visit);
        }
        Command::Case {
            word,
            arms,
            redirects,
        } => {
            visit(word);
            for (patterns, body) in arms {
                patterns.iter().for_each(&mut *visit);
                visit_words(body, visit);
            }
            redirect_words(redirects, visit);
        }
        Command::Conditional(words) => words.iter().for_each(&mut *visit),
        Command::Arithmetic(word) => visit(word),
        Command::Function { body, .. } => command_words(body, visit),
        Command::Unparsed(text) => visit(&Word::literal(text)),
    }
}
