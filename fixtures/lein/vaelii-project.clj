;; An excerpt of vaelii's project.clj (develop baa21c41): its layout, comments and
;; selectors, for `greenshift init lein`'s merge tests. Not a working project.
;; Defines `jdk-major`, `bash` and `test-jvm-opts`, which the `~` forms below read.
(load-file (str (.getParent (java.io.File. *file*)) "/project-env.clj"))

(defproject com.vaelii/vaelii "0.34.1-SNAPSHOT"
  :description "Vaelii — a contextualized common-sense knowledge base with a
                count-aware trie index, forward/backward inference,
                and JTMS truth maintenance, over an in-memory or on-disk store."
  :url "https://github.com/vaelii/vaelii"
  :license {:name "SSPL-1.0"
            :url "https://www.mongodb.com/licensing/server-side-public-license"}
  ;; Five marks, each deferring a test for its own reason: `^:slow` a test costing
  ;; about a second or more, `^:multi-jvm` one that forks a second JVM, `^:fuzz` the
  ;; exhaustive truncation sweep, `^:full-kb` a probe of a full-size KB on disk, and
  ;; `^:red` a test asserting behaviour the engine does not have yet, which fails until
  ;; that behaviour lands and then loses the mark (`lein test-red` fails on one that
  ;; passes). What each selects: CONTRIBUTING.md §5.
  ;; **Four of the five are opt-in only**, so neither `:default` nor `:all` selects
  ;; them and every other selector has to exclude them by name. `:all` is therefore
  ;; not `(complement :multi-jvm)`: a complement of one mark silently adopts the next
  ;; one added, which is how a forked JVM would end up in `lein gate`'s run.
  ;; `[m & _]`, not `#(… %)`: `lein test :slow some.ns` hands the selector the trailing
  ;; `some.ns` as an argument — leiningen splits namespaces (symbols) from selectors and
  ;; passes the var metadata first, then any tokens after the selector — so a one-arg
  ;; selector throws ArityException there. Swallow the rest; filter on the metadata alone.
  ;; **A bare keyword is not a shorthand for that**, and it fails the same way and
  ;; silently: `(:fuzz m "some.ns")` is a lookup with a DEFAULT, so the trailing
  ;; namespace becomes the answer for every test whose metadata lacks the key, and
  ;; `lein test :fuzz some.ns` runs the whole suite instead of one marked namespace.
  ;; Every selector here is a fn for that reason.
  :test-selectors {:default   (fn [m & _] (not (or (:slow m) (:multi-jvm m) (:fuzz m) (:full-kb m) (:red m))))
                   :slow      (fn [m & _] (and (:slow m) (not (or (:multi-jvm m) (:fuzz m) (:full-kb m) (:red m)))))
                   :multi-jvm (fn [m & _] (boolean (:multi-jvm m)))
                   :fuzz      (fn [m & _] (boolean (:fuzz m)))
                   :full-kb   (fn [m & _] (boolean (:full-kb m)))
                   :red       (fn [m & _] (boolean (:red m)))
                   :all       (fn [m & _] (not (or (:multi-jvm m) (:fuzz m) (:full-kb m) (:red m))))}
  ;; Leiningen's `:base` profile supplies the project JVM's `:jvm-opts` when the
  :jvm-opts ~(cond-> ["-XX:-OmitStackTraceInFastThrow" "--enable-native-access=ALL-UNNAMED"]
               (<= 23 jdk-major) (conj "--sun-misc-unsafe-memory-access=allow")
               (<= 25 jdk-major) (conj "-XX:+UseCompactObjectHeaders"))
  :profiles {:test {:dependencies [[org.clojure/test.check "1.1.3"]]
                    :injections
                    [(require 'vaelii.impl.logging 'vaelii.impl.config)
                     ;; a } in a comment, a "string with ] in it", \{ and #"\[re" too
                     (re-find #"[{}\"]+" "{\"")
                     #_{:discarded :map}
                     (println \; \" \newline \} ^:meta [] #?(:clj 1) #:a{:b 2} #inst "2026-01-01")]}})
