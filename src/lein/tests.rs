use super::*;

/// The excerpt of vaelii's project.clj the merge goldens start from.
const VAELII: &str = include_str!("../../fixtures/lein/vaelii-project.clj");

/// The entry exactly as the scope card (greenshift#35) shows the vaelii
/// maintainer would see it, with the map's keys at column 19. Written out
/// here rather than taken from `selector.clj`, so a change to the
/// template fails this test.
const CARD_ENTRY: &str = r#"                   ;; Added by `greenshift init lein`: exactly the tests named, one ns/var
                   ;; per line, in the file -Dgreenshift.names gives (`greenshift run lein`
                   ;; writes it). Plain `lein test` never reads it.
                   :greenshift (let [names (delay (->> (.split ^String (slurp (or (System/getProperty "greenshift.names")
                                                                                  (throw (ex-info "lein test :greenshift needs -Dgreenshift.names=FILE" {}))))
                                                               "\n")
                                                       (remove empty?) set))
                                     nss   (delay (set (keep #(let [i (.indexOf ^String % "/")] (when (pos? i) (subs ^String % 0 i))) @names)))]
                                 [(fn [ns & _] (contains? @nss (str ns)))
                                  (fn [m & _] (contains? @names (str (ns-name (:ns m)) "/" (:name m))))])"#;

/// The `:all` selector line, the last entry of vaelii's map, without its
/// closing brace.
const VAELII_LAST: &str =
    "                   :all       (fn [m & _] (not (or (:multi-jvm m) (:fuzz m) (:full-kb m) (:red m))))";

fn changed(merge: Merge) -> String {
    match merge {
        Merge::Changed(text) => text,
        other => panic!("not changed: {other:?}"),
    }
}

#[test]
fn the_reader_skips_strings_comments_characters_and_reader_macros() {
    let text = r#"; a ( comment
(a "b ) \" c" \) \( \" \newline #"re\"[" #_(dropped) ^:m x 'q `q ~u ~@us @d #'v
 #{1} #(f %) #?(:clj 1) #?@(:clj [1]) #:ns{:a 1} #inst "2026" ##Inf ; trailing (
 [1 2] {:k v})
"#;
    let forms = read_forms(text).unwrap();
    assert_eq!(forms.len(), 1);
    let children = forms[0].children();
    let texts: Vec<&str> = children.iter().map(|form| form.text(text)).collect();
    assert_eq!(
        texts,
        [
            "a",
            r#""b ) \" c""#,
            r"\)",
            r"\(",
            r#"\""#,
            r"\newline",
            r#"#"re\"[""#,
            "^:m x",
            "'q",
            "`q",
            "~u",
            "~@us",
            "@d",
            "#'v",
            "#{1}",
            "#(f %)",
            "#?(:clj 1)",
            "#?@(:clj [1])",
            "#:ns{:a 1}",
            r#"#inst "2026""#,
            "##Inf",
            "[1 2]",
            "{:k v}",
        ]
    );
}

#[test]
fn older_metadata_is_one_form_with_what_it_applies_to() {
    // Read as two forms, #^{…} would shift every option after it out of
    // its pair, and the merge would add a second :test-selectors, which
    // lein takes over the first.
    let text = "(defproject a \"1\"\n  :jvm-opts #^{:replace true} [\"-Xmx1g\"]\n  \
                :test-selectors {:fast :fast})";
    let forms = read_forms(text).unwrap();
    assert_eq!(
        forms[0].children()[4].text(text),
        "#^{:replace true} [\"-Xmx1g\"]"
    );
    assert!(matches!(selectors(text).unwrap(), Selectors::Literal(_)));
    let merged = changed(merge(text, false).unwrap());
    assert_eq!(merged.matches(":test-selectors").count(), 1);
    assert!(merged.contains(":test-selectors {:fast :fast\n"));
}

#[test]
fn unbalanced_or_unterminated_text_is_an_error_naming_its_line() {
    for (text, line) in [
        ("(a\n(b)", 1),
        ("\n\n)", 3),
        ("(a \"b)", 1),
        ("(a\n #_)", 2),
        ("(a \\", 1),
    ] {
        let err = read_forms(text).unwrap_err();
        assert_eq!(err.line, line, "{text:?}: {err}");
    }
}

#[test]
fn a_literal_map_is_read_and_a_computed_one_is_not() {
    assert!(matches!(
        selectors(VAELII).unwrap(),
        Selectors::Literal(map) if map.children().len() == 14
    ));
    for computed in [
        "(defproject a \"1\" :test-selectors ~(merge {} {}))",
        "(defproject a \"1\" :test-selectors selectors)",
        "(defproject a \"1\" :test-selectors)",
    ] {
        assert_eq!(
            selectors(computed).unwrap(),
            Selectors::Computed,
            "{computed}"
        );
        assert_eq!(merge(computed, true).unwrap_err(), ProjectError::Computed);
        assert!(!has_selector(computed).unwrap());
    }
    assert!(matches!(
        selectors("(defproject a \"1\"\n  :url \"u\")").unwrap(),
        Selectors::Absent { indent: 2, .. }
    ));
}

#[test]
fn only_the_defprojects_own_selectors_count() {
    // A :test-selectors in a profile, or in a comment form, is not the
    // project's.
    let text = "(comment :test-selectors {:greenshift 1})\n\
                (defproject a \"1\" :profiles {:x {:test-selectors {:greenshift 1}}})";
    assert!(matches!(selectors(text).unwrap(), Selectors::Absent { .. }));
    assert!(!has_selector(text).unwrap());
    assert_eq!(
        selectors("(def x 1)").unwrap_err(),
        ProjectError::NoDefproject
    );
    assert_eq!(
        selectors("(defproject a \"1\")(defproject b \"1\")").unwrap_err(),
        ProjectError::TwoDefprojects
    );
    assert_eq!(
        selectors("(defproject a \"1\" :test-selectors {} :test-selectors {})").unwrap_err(),
        ProjectError::TwoSelectorMaps
    );
}

#[test]
fn vaelii_gets_exactly_the_cards_entry_after_its_last_selector() {
    let merged = changed(merge(VAELII, false).unwrap());
    let before = format!("{VAELII_LAST}}}");
    let after = format!("{VAELII_LAST}\n{CARD_ENTRY}}}");
    assert_eq!(VAELII.matches(&before).count(), 1);
    assert_eq!(merged, VAELII.replacen(&before, &after, 1));
    assert!(has_selector(&merged).unwrap());
    assert!(!has_selector(VAELII).unwrap());
}

#[test]
fn a_second_merge_changes_nothing() {
    let merged = changed(merge(VAELII, false).unwrap());
    assert_eq!(merge(&merged, false).unwrap(), Merge::Unchanged);
    assert_eq!(merge(&merged, true).unwrap(), Merge::Unchanged);
}

#[test]
fn a_different_entry_is_a_conflict_unless_forced_and_force_replaces_only_its_value() {
    let merged = changed(merge(VAELII, false).unwrap());
    let edited = merged.replace("(remove empty?) set))", "(remove empty?) (into #{})))");
    assert_ne!(edited, merged);
    assert_eq!(merge(&edited, false).unwrap(), Merge::Conflict);
    assert_eq!(changed(merge(&edited, true).unwrap()), merged);
    // A hand-written entry keeps its own comment when forced.
    let own = VAELII.replacen(
        &format!("{VAELII_LAST}}}"),
        &format!("{VAELII_LAST}\n                   ;; mine\n                   :greenshift (constantly true)}}"),
        1,
    );
    assert_eq!(merge(&own, false).unwrap(), Merge::Conflict);
    let forced = changed(merge(&own, true).unwrap());
    assert!(forced.contains(";; mine\n                   :greenshift (let [names"));
    assert_eq!(merge(&forced, false).unwrap(), Merge::Unchanged);
}

#[test]
fn with_no_map_one_is_added_after_the_last_option() {
    let text = "(defproject a \"1\"\n  :dependencies [[org.clojure/clojure \"1.12.6\"]])\n";
    let merged = changed(merge(text, false).unwrap());
    let card_lines: Vec<&str> = CARD_ENTRY.lines().collect();
    // The comments at the options' column, the key inside the new map at
    // the card's column 19 (2 + ":test-selectors {".len()).
    let mut expected =
        String::from("(defproject a \"1\"\n  :dependencies [[org.clojure/clojure \"1.12.6\"]]\n");
    for line in &card_lines[..3] {
        expected.push_str("  ");
        expected.push_str(line.trim_start());
        expected.push('\n');
    }
    expected.push_str("  :test-selectors {");
    expected.push_str(card_lines[3].trim_start());
    for line in &card_lines[4..] {
        expected.push('\n');
        expected.push_str(line);
    }
    expected.push_str("})\n");
    assert_eq!(merged, expected);
    assert!(has_selector(&merged).unwrap());
    assert_eq!(merge(&merged, false).unwrap(), Merge::Unchanged);
}

#[test]
fn a_trailing_comment_stays_with_the_entry_before_it() {
    let text =
        "(defproject a \"1\"\n  :test-selectors {:fast :fast ; quick ones\n                   })";
    let merged = changed(merge(text, false).unwrap());
    assert!(merged.starts_with(
        "(defproject a \"1\"\n  :test-selectors {:fast :fast ; quick ones\n                   ;; Added by"
    ));
    assert!(merged.ends_with("(:name m))))])\n                   })"));
    assert!(has_selector(&merged).unwrap());
    assert_eq!(merge(&merged, false).unwrap(), Merge::Unchanged);
}

#[test]
fn an_empty_map_gets_the_entry_inside_its_brace() {
    let text = "(defproject a \"1\" :test-selectors {})";
    let merged = changed(merge(text, false).unwrap());
    assert!(merged.starts_with("(defproject a \"1\" :test-selectors {;; Added by"));
    assert!(merged.ends_with("(:name m))))])})"));
    assert!(has_selector(&merged).unwrap());
    assert_eq!(merge(&merged, false).unwrap(), Merge::Unchanged);
}

#[test]
fn the_selector_value_lines_up_under_its_key() {
    let value = selector_value(19);
    let card_value = CARD_ENTRY.lines().skip(3).collect::<Vec<_>>().join("\n");
    assert_eq!(format!("{}:greenshift {value}", " ".repeat(19)), card_value);
}

#[test]
fn clojure_strings_escape_quotes_backslashes_and_newlines() {
    assert_eq!(clojure_string("a/b"), r#""a/b""#);
    assert_eq!(clojure_string("q\"b\\n\nr\rt\t"), r#""q\"b\\n\nr\rt\t""#);
}
