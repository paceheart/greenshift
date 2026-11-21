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
;; - Every name in the names file that never ran is <skipped>: after a
;;   fixture error in its namespace, "not run: fixture error in <ns>", and
;;   otherwise "not run". A var that began but never ended (the JVM exited
;;   inside it) is an <error>.
;; - The file is written when run-tests returns, before lein's System/exit,
;;   with a shutdown hook as a backstop for a JVM that exits inside the
;;   run. If run-tests never started (a namespace failed to load), no file
;;   is written, so greenshift records the run as partial rather than as
;;   tests that were skipped.
;;
;; Every hook chains to what was there before (get-method, alter-var-root),
;; so a project's own clojure.test hooks keep working.
(ns greenshift.lein.junit
  (:require [clojure.string :as str]
            [clojure.test :as t]))

;; JUnit record: {:ns :name :nanos :pass :fail :error :problems :skipped}, in run order.
(def ^:private cases (atom []))
(def ^:private current (atom nil))      ; index into @cases of the var now running, or nil
(def ^:private started (atom nil))      ; System/nanoTime when the var now running began
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
  (let [fixture (when-not @current @current-ns)]
    (when-let [i (or @current (some-> fixture fixtures-index!))]
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
  "The cases recorded, a var that never ended as an error, then a skipped
  case for every name in names that has none."
  [names]
  (let [cs (cond-> @cases
             @current (update @current
                              #(-> %
                                   (assoc :nanos (- (System/nanoTime) (or @started (System/nanoTime))))
                                   (update :error inc)
                                   (update :problems conj
                                           {:kind :error :message "did not finish: the JVM exited during it"
                                            :type "exit" :body "The test began and never ended."}))))
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

(defn install!
  "Record every test var's result and write them as JUnit to junit when
  the run ends; run the namespaces in the order the names file first
  names them, the rest after, as given."
  [{:keys [^String names ^String junit]}]
  (let [names (read-names names)
        order (reduce (fn [m ^String n]
                        (let [i (.indexOf n "/")
                              k (if (pos? i) (subs n 0 i) n)]
                          (if (contains? m k) m (assoc m k (count m)))))
                      {} names)
        default (get-method t/report :default)
        orig (fn [k] (or (get-method t/report k) default))
        begin (orig :begin-test-var)
        end (orig :end-test-var)
        end-ns (orig :end-test-ns)]
    (defmethod t/report :begin-test-var [m]
      (reset! started (System/nanoTime))
      (swap! cases conj (case-of (:var m)))
      (reset! current (dec (count @cases)))
      (begin m))
    (defmethod t/report :end-test-var [m]
      (when-let [i @current]
        (swap! cases assoc-in [i :nanos] (- (System/nanoTime) (or @started (System/nanoTime))))
        (reset! current nil))
      (end m))
    (defmethod t/report :end-test-ns [m]
      (reset! current-ns nil)
      (end-ns m))
    ;; Record each assertion as well as counting it, keeping clojure.test's own report.
    (doseq [[k kind] [[:pass nil] [:fail :failure] [:error :error]]]
      (let [o (orig k)]
        (defmethod t/report k [m]
          (note! k (when kind (problem m k kind)))
          (o m))))
    (alter-var-root #'t/test-ns
                    (fn [test-ns]
                      (fn [ns]
                        (reset! current-ns (ns-str ns))
                        (test-ns ns))))
    (alter-var-root
     #'t/run-tests
     (fn [run-tests]
       (fn [& namespaces]
         (let [nss (sort-by #(get order (ns-str %) Long/MAX_VALUE)
                            (or (seq namespaces) [*ns*]))]
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
               (write! junit names)))))))
    (.addShutdownHook (Runtime/getRuntime)
                      (Thread. ^Runnable (fn [] (write! junit names))))
    :installed))
