//! `ask_user`: put a question to the person at the keyboard and wait.

use ah_abi::{Answer, Ask, Choice, Question, Reply, ToolResult, ToolSpec};
use serde_json::{Value, json};

use super::{Tool, ToolCtx};

/// Questions in one call. More than a handful is an interrogation, not a
/// question, and the user has to hold every answer in their head at once.
const MAX_QUESTIONS: usize = 4;
/// Options per question, for the same reason.
const MAX_OPTIONS: usize = 8;

pub struct AskTool;

impl Tool for AskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "ask_user",
            "Ask the user something when their answer changes what you do next and nothing \
             else can settle it: the request is ambiguous in a way that leads to different \
             work, or the choice is theirs to make. Offer the concrete options you would \
             otherwise pick between; they can always type an answer of their own instead, \
             so the options are a shortcut, not a fence. Put your recommendation first. \
             Do not use this to ask permission for work you were already asked to do, to \
             report progress, or for anything you can find out by reading the code. Asking \
             costs the user a stop, so ask only what you are actually blocked on, and ask \
             it once.",
            json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "One to four questions, asked one after another",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string", "description": "The question, in full"},
                                "header": {"type": "string", "description": "Two or three words naming the decision"},
                                "options": {
                                    "type": "array",
                                    "description": "Up to eight answers to choose from; omit when the answer is free text",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string"},
                                            "description": {"type": "string", "description": "What picking it means"}
                                        },
                                        "required": ["label"]
                                    }
                                },
                                "multi": {"type": "boolean", "description": "Options are not exclusive; the user may pick several"}
                            },
                            "required": ["question"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let ask = match parse(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(e),
        };
        if ctx.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return ToolResult::err("the turn was interrupted before the question was asked");
        }
        match ctx.ask.ask(&ask) {
            Reply::Answered { answers } if answers.len() == ask.questions.len() => {
                ToolResult::ok(report(&ask, &answers))
            }
            // The UI answers every question or none; a short reply is a bug
            // rather than an answer, and guessing which question it belongs
            // to would be worse than saying so.
            Reply::Answered { .. } => ToolResult::err("the answer did not fit the questions"),
            Reply::Dismissed => ToolResult::err(
                "the user closed the question without answering. Do not ask again: decide it \
                 yourself, say which way you went and why, and carry on.",
            ),
            Reply::Unavailable => ToolResult::err(
                "there is nobody to ask; this run has no interactive user. Decide it yourself, \
                 state the assumption you are working under, and carry on.",
            ),
        }
    }
}

/// The model's arguments as questions. Models phrase this call in more than one
/// way, so a single question outside a list, a question written as a plain
/// string and options written as plain strings are all read rather than
/// refused; anything that would reach the user malformed is not.
fn parse(args: &Value) -> Result<Ask, String> {
    let items: Vec<&Value> = match args.get("questions") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v @ (Value::Object(_) | Value::String(_))) => vec![v],
        Some(_) => return Err("`questions` is a list of questions".into()),
        // `{"question": ..., "options": [...]}` on its own is one question.
        None if first_str(args, &["question", "prompt", "text"]).is_some() => vec![args],
        None => return Err("missing `questions`".into()),
    };
    if items.is_empty() {
        return Err("`questions` is empty".into());
    }
    if items.len() > MAX_QUESTIONS {
        return Err(format!(
            "{} questions; ask at most {MAX_QUESTIONS} at a time",
            items.len()
        ));
    }
    let mut questions = Vec::with_capacity(items.len());
    for (i, v) in items.iter().enumerate() {
        questions.push(question(v).map_err(|e| format!("question {}: {e}", i + 1))?);
    }
    Ok(Ask { questions })
}

fn question(v: &Value) -> Result<Question, String> {
    if let Some(s) = v.as_str() {
        return match s.trim() {
            "" => Err("no text".into()),
            t => Ok(Question {
                question: t.to_string(),
                ..Default::default()
            }),
        };
    }
    if !v.is_object() {
        return Err("expected an object with `question`".into());
    }
    let text = first_str(v, &["question", "prompt", "text"])
        .unwrap_or("")
        .trim();
    if text.is_empty() {
        return Err("no text".into());
    }
    let header = first_str(v, &["header", "label", "topic", "title"])
        .unwrap_or("")
        .trim();
    Ok(Question {
        header: header.to_string(),
        question: text.to_string(),
        options: options(v)?,
        multi: flag(v, &["multi", "multiSelect", "multi_select", "multiple"]),
    })
}

fn options(v: &Value) -> Result<Vec<Choice>, String> {
    let Some(list) = v.get("options").or_else(|| v.get("choices")) else {
        return Ok(Vec::new());
    };
    let Some(list) = list.as_array() else {
        return Err("`options` is a list".into());
    };
    if list.len() > MAX_OPTIONS {
        return Err(format!(
            "{} options; offer at most {MAX_OPTIONS}",
            list.len()
        ));
    }
    let mut out: Vec<Choice> = Vec::with_capacity(list.len());
    for item in list {
        let (label, description) = match item {
            Value::String(s) => (s.trim(), ""),
            Value::Object(_) => (
                first_str(item, &["label", "name", "text", "value", "option"])
                    .unwrap_or("")
                    .trim(),
                first_str(item, &["description", "detail", "hint", "subtitle"])
                    .unwrap_or("")
                    .trim(),
            ),
            _ => ("", ""),
        };
        // A blank or repeated option is a row the user cannot tell apart from
        // its neighbour, so it never reaches the screen.
        if label.is_empty() || out.iter().any(|c| c.label.eq_ignore_ascii_case(label)) {
            continue;
        }
        out.push(Choice {
            label: label.to_string(),
            description: description.to_string(),
        });
    }
    Ok(out)
}

fn first_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str))
}

fn flag(v: &Value, keys: &[&str]) -> bool {
    keys.iter().any(|k| {
        v.get(*k).is_some_and(|f| {
            f.as_bool()
                .unwrap_or_else(|| matches!(f.as_str(), Some("true")))
        })
    })
}

/// The answers as the model reads them: each question, then what came back.
fn report(ask: &Ask, answers: &[Answer]) -> String {
    let mut out = String::new();
    for (q, a) in ask.questions.iter().zip(answers) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(q.question.trim());
        out.push_str("\nanswer: ");
        if a.picked.is_empty() {
            out.push_str(a.note.trim());
        } else {
            out.push_str(&a.picked.join(", "));
            if !a.note.trim().is_empty() {
                out.push_str("\nnote: ");
                out.push_str(a.note.trim());
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AskUser;

    struct Canned(Reply);
    impl AskUser for Canned {
        fn ask(&self, _: &Ask) -> Reply {
            self.0.clone()
        }
    }

    fn run(args: Value, reply: Reply) -> ToolResult {
        let cwd = std::env::current_dir().unwrap();
        let settings = ah_abi::ToolSettings::default();
        let asker = Canned(reply);
        let ctx = ToolCtx {
            cwd: &cwd,
            settings: &settings,
            agent: 0,
            cancel: crate::tools::never(),
            ask: &asker,
        };
        AskTool.run(&args, &ctx)
    }

    fn answered(picked: &[&str], note: &str) -> Reply {
        Reply::Answered {
            answers: vec![Answer {
                picked: picked.iter().map(|s| s.to_string()).collect(),
                note: note.to_string(),
            }],
        }
    }

    #[test]
    fn a_question_is_asked_and_the_answer_comes_back_as_text() {
        let r = run(
            json!({"questions": [{
                "header": "Auth",
                "question": "Which auth method?",
                "options": [{"label": "OAuth", "description": "no secret stored"}, {"label": "API key"}]
            }]}),
            answered(&["OAuth"], "use PKCE"),
        );
        assert!(!r.is_error, "{}", r.output);
        assert_eq!(
            r.output,
            "Which auth method?\nanswer: OAuth\nnote: use PKCE\n"
        );
    }

    #[test]
    fn the_shapes_a_model_reaches_for_are_all_read() {
        // One question outside a list, options as plain strings, the other
        // spelling of `multi`.
        let ask = parse(&json!({
            "question": "Which ones?",
            "options": ["a", "b", " a ", ""],
            "multiSelect": true
        }))
        .unwrap();
        assert_eq!(ask.questions.len(), 1);
        let q = &ask.questions[0];
        assert!(q.multi);
        assert_eq!(q.options.len(), 2, "blank and repeated options are dropped");
        assert_eq!(q.options[0].label, "a");
        // A question written as a bare string, and a list of them.
        let ask = parse(&json!({"questions": ["first?", {"question": "second?"}]})).unwrap();
        assert_eq!(ask.questions.len(), 2);
        assert_eq!(ask.questions[0].question, "first?");
        assert!(ask.questions[0].options.is_empty());
    }

    #[test]
    fn a_question_that_cannot_be_asked_says_why() {
        for (args, want) in [
            (json!({}), "missing `questions`"),
            (json!({"questions": []}), "`questions` is empty"),
            (json!({"questions": [{"question": "  "}]}), "no text"),
            (
                json!({"questions": [{"question": "q", "options": {"a": 1}}]}),
                "`options` is a list",
            ),
            (
                json!({"questions": ["a?", "b?", "c?", "d?", "e?"]}),
                "at most 4",
            ),
            (
                json!({"questions": [{"question": "q", "options": ["1","2","3","4","5","6","7","8","9"]}]}),
                "at most 8",
            ),
        ] {
            let e = parse(&args).unwrap_err();
            assert!(e.contains(want), "{args} gave {e:?}, wanted {want:?}");
        }
    }

    #[test]
    fn a_question_nobody_answers_tells_the_model_to_get_on_with_it() {
        let q = json!({"questions": [{"question": "Which one?"}]});
        let r = run(q.clone(), Reply::Dismissed);
        assert!(r.is_error);
        assert!(r.output.contains("Do not ask again"), "{}", r.output);
        let r = run(q.clone(), Reply::Unavailable);
        assert!(r.is_error);
        assert!(r.output.contains("nobody to ask"), "{}", r.output);
        // Free text alone is a whole answer.
        let r = run(q, answered(&[], "neither, use both"));
        assert_eq!(r.output, "Which one?\nanswer: neither, use both\n");
    }
}
