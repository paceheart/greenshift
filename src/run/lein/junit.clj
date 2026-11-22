;; greenshift's JUnit reporter for `greenshift run lein`: loaded into `lein
;; test`'s JVM as an injection, it writes one JUnit XML file for the run,
;; and orders the namespaces lein runs by their first appearance in the
;; names file greenshift wrote for the run. lein's own output, and its exit
;; status, stay as they are.
;;
;; Ported from vaelii-lacuna's tools/vaelii-run-vars.clj (vaelii-lacuna#81)
;; and adapted to `lein test`, which runs the tests itself:
;;
;; - One <testcase> per test var: classname the namespace, name the var,
;;   time in seconds from System/nanoTime, assertions that var's pass, fail
;;   and error count; a <failure> per failed assertion and an <error> per
;;   exception.
;; - The namespace running now is the one clojure.test/test-ns was last
;;   called with: lein's own hook on test-ns hides :begin-test-ns from
;;   report methods, so test-ns is wrapped instead.
;; - An assertion outside any var (in a :once or :each fixture), and a
;;   fixture's exception (which lein catches and reports as an error), go on
;;   a testcase named "fixtures" in that namespace, so the file's failures
;;   and errors agree with clojure.test's counts.
;; - A namespace the names file names runs even when lein did not select
;;   it (lein selects one only for a public test, so a group of its private
;;   tests alone would never run), its unnamed tests set aside meanwhile;
;;   one lein never loaded (outside :test-paths, named to the listing) is
;;   loaded first.
;; - Every name in the names file that never ran is <skipped>: after a
;;   fixture error in its namespace, "not run: fixture error in <ns>", and
;;   otherwise "not run". A var that began but never ended (the JVM exited
;;   inside it) is an <error>.
;; - The file is written when run-tests returns, before lein's System/exit,
;;   with a shutdown hook as a backstop for a JVM that exits inside the
;;   run. If run-tests never started (a namespace failed to load), no file
;;   is written, so greenshift records the run as partial (or, when no
;;   group wrote a file, not at all) rather than as tests that were
;;   skipped.
;;
;; Every hook chains to what was there before (get-method, alter-var-root),
;; so a project's own clojure.test hooks keep working. A report method that
;; a test namespace defines when it loads, after this file, replaces
;; greenshift's; run-tests, called once every namespace has loaded, hooks
;; such a method again, chaining to it. When a project's own injection has
;; made clojure.test/report a plain function, hiding the multimethod, the
;; reporter wraps that function instead.
(ns greenshift.lein.junit
  (:require [clojure.string :as str]
            [clojure.test :as t]))

;; JUnit record: {:ns :name :start :nanos :pass :fail :error :problems :skipped}, in run order.
(def ^:private cases (atom []))
;; Indexes into @cases of the vars now running, innermost last: a deftest that
;; calls another (clojure.test's way to compose tests) runs that one inside it.
(def ^:private current (atom []))
(def ^:private current-ns (atom nil))   ; name of the namespace now running, or nil
(def ^:private fixture-errors (atom #{})) ; namespaces whose fixtures threw
(def ^:private run-started (atom nil))  ; System/nanoTime when run-tests began, or nil
(def ^:private written (atom false))

(defn- ns-str
  "The name of a namespace, or of the symbol naming one."
  ^String [x]
  (str (if (instance? clojure.lang.Namespace x) (ns-name x) x)))

(defn- case-of [v]
  (let [m (meta v)]
    {:ns (str (ns-name (:ns m))) :name (str (:name m)) :pass 0 :fail 0 :error 0 :problems []}))

(defn- stack-trace [^Throwable t]
  (let [w (java.io.StringWriter.)]
    (.printStackTrace t (java.io.PrintWriter. w true))
    (str w)))

;; The "fixtures" testcase of a namespace, added the first time it is needed.
(defn- fixtures-index! [ns-name-str]
  (or (first (keep-indexed (fn [i c] (when (and (= ns-name-str (:ns c)) (= "fixtures" (:name c))) i))
                           @cases))
      (do (swap! cases conj {:ns ns-name-str :name "fixtures" :pass 0 :fail 0 :error 0 :problems []})
          (dec (count @cases)))))

(defn- note!
  "Count an assertion of kind k, with its problem if it failed, on the var
  now running, or else on the running namespace's fixtures testcase."
  [k problem]
  (let [running (peek @current)
        fixture (when-not running @current-ns)]
    (when-let [i (or running (some-> fixture fixtures-index!))]
      (when (and fixture (= k :error))
        (swap! fixture-errors conj fixture))
      (swap! cases #(-> %
                        (update-in [i k] (fnil inc 0))
                        (cond-> problem (update-in [i :problems] conj problem)))))))

(defn- problem [m k kind]
  {:kind kind
   ;; an uncaught exception carries clojure.test's generic :message, so the
   ;; exception's own message comes first
   :message (or (when (instance? Throwable (:actual m))
                  (.getMessage ^Throwable (:actual m)))
                (:message m)
                (name k))
   :type (if (= kind :failure)
           "assertion"
           (if (instance? Throwable (:actual m))
             (.getName (class (:actual m)))
             "error"))
   :body (str (when (seq t/*testing-contexts*)
                (str (t/testing-contexts-str) "\n"))
              (when (and (:message m) (instance? Throwable (:actual m)))
                (str (:message m) "\n"))
              "expected: " (pr-str (:expected m)) "\n"
              "  actual: " (if (instance? Throwable (:actual m))
                             (stack-trace (:actual m))
                             (pr-str (:actual m))))})

;; Characters XML 1.0 forbids (C0 controls but tab, newline and carriage return, and
;; U+FFFE/U+FFFF) are written as the text \uXXXX, so an ANSI colour code in a message
;; or a printed value still yields a file that parses.
(defn- xml-escape ^String [s]
  (-> (str s)
      (str/replace #"[\x00-\x08\x0B\x0C\x0E-\x1F\uFFFE\uFFFF]"
                   #(format "\\u%04x" (int (first %))))
      (str/replace "&" "&amp;") (str/replace "<" "&lt;") (str/replace ">" "&gt;")
      (str/replace "\"" "&quot;") (str/replace "'" "&apos;")))

;; In an attribute value a parser folds a raw tab, newline or carriage return into a
;; space, so those are written as character references.
(defn- xml-attr ^String [s]
  (-> (xml-escape s)
      (str/replace "\t" "&#9;") (str/replace "\n" "&#10;") (str/replace "\r" "&#13;")))

;; Locale/ROOT: a decimal point whatever the JVM's default locale.
(defn- seconds ^String [nanos]
  (String/format java.util.Locale/ROOT "%.6f" (object-array [(/ (double (or nanos 0)) 1e9)])))

(defn- testcase ^String [{:keys [ns name nanos pass fail error problems skipped]}]
  (str "    <testcase classname=\"" (xml-attr ns) "\" name=\"" (xml-attr name)
       "\" time=\"" (seconds nanos) "\" assertions=\"" (+ (or pass 0) (or fail 0) (or error 0)) "\""
       (if (or skipped (seq problems))
         (str ">\n"
              (when skipped
                (str "      <skipped message=\"" (xml-attr skipped) "\"/>\n"))
              (apply str
                     (for [{:keys [kind message type body]} problems]
                       (str "      <" (clojure.core/name kind) " message=\"" (xml-attr message)
                            "\" type=\"" (xml-attr type) "\">" (xml-escape body)
                            "</" (clojure.core/name kind) ">\n")))
              "    </testcase>\n")
         "/>\n")))

(defn- finished-cases
  "The cases recorded, every var that never ended as an error, then a
  skipped case for every name in names that has none."
  [names]
  (let [cs (reduce (fn [cs i]
                     (update cs i
                             #(-> %
                                  (assoc :nanos (- (System/nanoTime) (:start %)))
                                  (update :error inc)
                                  (update :problems conj
                                          {:kind :error :message "did not finish: the JVM exited during it"
                                           :type "exit" :body "The test began and never ended."}))))
                   @cases @current)
        ran (set (map (juxt :ns :name) cs))
        failed-ns @fixture-errors]
    (into cs
          (for [n names
                :let [i (.indexOf ^String n "/")
                      ns-part (if (pos? i) (subs n 0 i) "")
                      var-part (if (pos? i) (subs n (inc i)) n)]
                :when (not (ran [ns-part var-part]))]
            {:ns ns-part :name var-part :pass 0 :fail 0 :error 0 :problems []
             :skipped (if (failed-ns ns-part)
                        (str "not run: fixture error in " ns-part)
                        "not run")}))))

(defn- write!
  "Write the JUnit file once, if run-tests started. A file that cannot be
  written is named on stderr and changes nothing else."
  [^String path names]
  (when (and @run-started (compare-and-set! written false true))
    (try
      (let [cs (finished-cases names)
            fails (count (filter #(some (comp #{:failure} :kind) (:problems %)) cs))
            errors (count (filter #(and (not-any? (comp #{:failure} :kind) (:problems %))
                                        (some (comp #{:error} :kind) (:problems %))) cs))
            skipped (count (filter :skipped cs))]
        (spit path
              (str "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
                   "<testsuites>\n"
                   "  <testsuite name=\"greenshift-lein\" tests=\"" (count cs) "\" failures=\"" fails
                   "\" errors=\"" errors "\" skipped=\"" skipped
                   "\" time=\"" (seconds (- (System/nanoTime) @run-started))
                   "\" timestamp=\"" (str (java.time.Instant/now)) "\">\n"
                   (apply str (map testcase cs))
                   "  </testsuite>\n"
                   "</testsuites>\n")))
      (catch Exception e
        (binding [*out* *err*]
          (println "greenshift: could not write JUnit file" path
                   (str "(" (.getName (class e)) ": " (.getMessage e) ")"))
          (flush))))))

(defn- read-names
  "The test names in the names file, one ns/var per line, in order."
  [^String path]
  (into [] (comp (remove str/blank?) (distinct)) (str/split-lines (slurp path))))

;; The kind of report one of greenshift's methods is recording now, or nil.
(def ^:private ^:dynamic *recording* nil)

(defn- hook!
  "Make report method k of the multimethod mf (wrap previous), previous
  being the method k has now, and note it in ours as k's. Reached while
  greenshift's newer method for k records (through a method that chains
  to the one it replaced, with that map or one of its own), a method
  that is no longer k's passes the report on unrecorded: it is recorded
  once already."
  [^clojure.lang.MultiFn mf ours k wrap]
  (let [previous (.getMethod mf k)
        record (wrap previous)
        f (fn self [m]
            (if (and (= k *recording*) (not (identical? self (.getMethod mf k))))
              (previous m)
              (binding [*recording* k]
                (record m))))]
    (.addMethod mf k f)
    (swap! ours assoc k [wrap f])))

(defn- rehook!
  "Hook again every method in ours that a later defmethod replaced (a
  test namespace's own, say, loaded after this file), chaining to it."
  [mf ours]
  (doseq [[k [wrap f]] @ours]
    (when-not (identical? f (.getMethod ^clojure.lang.MultiFn mf k))
      (hook! mf ours k wrap))))

(defn- require-unhooked!
  "Load the namespace sym as lein loads a test namespace: before lein hooks
  #'t/report, so with the multimethod mf (when it is one) as the var's
  root, and a defmethod in sym finds it; the hooked root comes back after."
  [mf sym]
  (if (instance? clojure.lang.MultiFn mf)
    (let [hooked @#'t/report]
      (alter-var-root #'t/report (constantly mf))
      (try (require sym)
           (finally (alter-var-root #'t/report (constantly hooked)))))
    (require sym)))

(defn install!
  "Record every test var's result and write them as JUnit to junit when
  the run ends; run the namespaces in the order the names file first
  names them, the rest after, as given."
  [{:keys [^String names ^String junit]}]
  (let [names (read-names names)
        wanted (set names)
        order (reduce (fn [m ^String n]
                        (let [i (.indexOf n "/")
                              k (if (pos? i) (subs n 0 i) n)]
                          (if (contains? m k) m (assoc m k (count m)))))
                      {} names)
        ;; The multimethod itself: lein later hooks the var, so run-tests
        ;; finds the methods through this, not through #'t/report.
        mf t/report
        ours (atom {})
        hooks [[:begin-test-var
                (fn [begin]
                  (fn [m]
                    (swap! cases conj (assoc (case-of (:var m)) :start (System/nanoTime)))
                    (swap! current conj (dec (count @cases)))
                    (begin m)))]
               [:end-test-var
                (fn [end]
                  (fn [m]
                    (when-let [i (peek @current)]
                      (swap! cases assoc-in [i :nanos] (- (System/nanoTime) (get-in @cases [i :start])))
                      (swap! current pop))
                    (end m)))]
               [:end-test-ns
                (fn [end-ns]
                  (fn [m]
                    (reset! current-ns nil)
                    (end-ns m)))]]
        ;; Record each assertion as well as counting it, keeping clojure.test's own report.
        hooks (into hooks
                    (for [[k kind] [[:pass nil] [:fail :failure] [:error :error]]]
                      [k (fn [o]
                           (fn [m]
                             (note! k (when kind (problem m k kind)))
                             (o m)))]))]
    (if (instance? clojure.lang.MultiFn mf)
      (doseq [[k wrap] hooks]
        (hook! mf ours k wrap))
      ;; A project's own injection made the report var a function of its
      ;; own, hiding the multimethod (test.check allows for such tools,
      ;; TCHECK-125): record around that function instead.
      (alter-var-root
       #'t/report
       (fn [report]
         (let [records (into {} (for [[k wrap] hooks] [k (wrap report)]))]
           (fn [m & more]
             (if-let [record (records (:type m))]
               (record m)
               (apply report m more)))))))
    (alter-var-root #'t/test-ns
                    (fn [test-ns]
                      (fn [ns]
                        (reset! current-ns (ns-str ns))
                        (test-ns ns))))
    (alter-var-root
     #'t/run-tests
     (fn [run-tests]
       (fn [& namespaces]
         (let [given (set (map ns-str namespaces))
               ;; lein selects a namespace only when a public var matches,
               ;; so a group naming only private tests of a namespace (its
               ;; public ones planned into the other group) would never
               ;; run them. Such a namespace, loaded, runs as well, with
               ;; every test the names file leaves out set aside, as lein
               ;; sets aside the tests its selectors leave out.
               ;; A named namespace lein never loaded (one the listing
               ;; was given that is outside :test-paths, which a run's
               ;; own discovery never finds) is loaded here, as `lein
               ;; test <ns>` loads it, so its tests run rather than
               ;; being skipped. Its own report methods are hooked
               ;; below with every other namespace's.
               extra (vec (keep #(when-not (given %)
                                   (let [sym (symbol %)]
                                     (or (find-ns sym)
                                         (do (require-unhooked! mf sym) (find-ns sym)))))
                                (keys order)))
               _ (rehook! mf ours)
               aside (vec (for [ns extra
                                v (vals (ns-interns ns))
                                :when (and (:test (meta v))
                                           (not (wanted (str (ns-name ns) "/" (:name (meta v))))))]
                            v))
               nss (sort-by #(get order (ns-str %) Long/MAX_VALUE)
                            (or (seq (concat namespaces extra)) [*ns*]))]
           (doseq [v aside]
             (alter-meta! v #(-> % (assoc ::set-aside (:test %)) (dissoc :test))))
           (reset! run-started (System/nanoTime))
           (try
             (apply run-tests nss)
             (catch Throwable e
               ;; A fixture's exception that lein did not catch (with
               ;; :monkeypatch-clojure-test false) ends the run here.
               (when-let [n @current-ns]
                 (let [i (fixtures-index! n)]
                   (swap! fixture-errors conj n)
                   (swap! cases #(-> %
                                     (update-in [i :error] inc)
                                     (update-in [i :problems] conj
                                                {:kind :error :message (str (.getMessage e))
                                                 :type (.getName (class e))
                                                 :body (stack-trace e)})))))
               (throw e))
             (finally
               (doseq [v aside]
                 (alter-meta! v #(-> % (assoc :test (::set-aside %)) (dissoc ::set-aside))))
               (write! junit names)))))))
    (.addShutdownHook (Runtime/getRuntime)
                      (Thread. ^Runnable (fn [] (write! junit names))))
    :installed))
