//! Chat formats that llama.cpp's built-in template families do not cover.
//!
//! `llama_chat_apply_template` recognises a fixed list of template families and
//! refuses the rest; full Jinja rendering lives only in llama.cpp `common`, which
//! this crate never links (it pulls in cpp-httplib). A model whose template is
//! refused is formatted here only when its template carries every marker of a
//! format we reproduce, for plain text turns only (no tools, images or thinking).

/// Markers that identify Google's Gemma 4 chat template ("Google Gemma 4
/// Canonical Chat Template", published 2026-07-09, embedded as
/// `tokenizer.chat_template` in `google/gemma-4-12B-it-qat-q4_0-gguf`).
const GEMMA4_MARKERS: [&str; 4] = [
    "'<|turn>system\\n'",
    "'<|turn>' + role + '\\n'",
    "'<turn|>\\n'",
    "'<|channel>thought\\n<channel|>'",
];

/// Whether `template` is Gemma 4's chat template.
pub(crate) fn is_gemma4(template: &str) -> bool {
    GEMMA4_MARKERS.iter().all(|m| template.contains(m))
}

/// Formats `(role, content)` turns as Gemma 4's template does with
/// `add_generation_prompt = true` and thinking off.
///
/// Follows the template's plain text path: a leading system message becomes the
/// system turn, `assistant` is renamed `model`, every content is trimmed (model
/// content passes `strip_thinking`, which for text without channel markers is a
/// trim), and the prompt ends with an open model turn whose thought channel is
/// empty. `bos_token` is left out because tokenization adds it, as for every
/// llama.cpp built-in template. Consecutive assistant messages are not merged
/// (the summarizer never sends them).
pub(crate) fn gemma4(messages: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let mut rest = messages;
    if let Some(((role, content), tail)) = messages.split_first()
        && matches!(*role, "system" | "developer")
    {
        out.push_str("<|turn>system\n");
        out.push_str(content.trim());
        out.push_str("<turn|>\n");
        rest = tail;
    }
    for (role, content) in rest {
        let role = if *role == "assistant" { "model" } else { role };
        out.push_str("<|turn>");
        out.push_str(role);
        out.push('\n');
        out.push_str(content.trim());
        out.push_str("<turn|>\n");
    }
    out.push_str("<|turn>model\n<|channel>thought\n<channel|>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected strings are derived by hand from the template's plain text path
    // (no Jinja renderer is available on the build host).

    #[test]
    fn chat_format_gemma4_system_and_user() {
        let got = gemma4(&[("system", " Be brief.\n"), ("user", "Hello")]);
        assert_eq!(
            got,
            "<|turn>system\nBe brief.<turn|>\n<|turn>user\nHello<turn|>\n<|turn>model\n<|channel>thought\n<channel|>"
        );
    }

    #[test]
    fn chat_format_gemma4_repair_round_renames_assistant() {
        let got = gemma4(&[
            ("system", "S"),
            ("user", "U"),
            ("assistant", "{\"a\": 1}\n"),
            ("user", "Fix it"),
        ]);
        assert_eq!(
            got,
            "<|turn>system\nS<turn|>\n<|turn>user\nU<turn|>\n<|turn>model\n{\"a\": 1}<turn|>\n<|turn>user\nFix it<turn|>\n<|turn>model\n<|channel>thought\n<channel|>"
        );
    }

    #[test]
    fn chat_format_gemma4_without_system() {
        assert_eq!(
            gemma4(&[("user", "Hi")]),
            "<|turn>user\nHi<turn|>\n<|turn>model\n<|channel>thought\n<channel|>"
        );
    }

    #[test]
    fn chat_format_gemma4_is_recognised_only_with_every_marker() {
        let all = GEMMA4_MARKERS.join(" ");
        assert!(is_gemma4(&all));
        for skip in 0..GEMMA4_MARKERS.len() {
            let some: Vec<&str> = GEMMA4_MARKERS
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, m)| *m)
                .collect();
            assert!(!is_gemma4(&some.join(" ")), "missing marker {skip}");
        }
        assert!(!is_gemma4(
            "{% for m in messages %}<|im_start|>{% endfor %}"
        ));
    }
}
