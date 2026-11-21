;; greenshift's listing for `greenshift run lein`: loaded into `lein test`'s
;; JVM as an injection, it makes `clojure.test/run-tests` list the tests it
;; was going to run instead of running them, so the listing is exactly
;; lein's own selection: its discovery, its selectors (namespace metadata,
;; trailing arguments, :default) and tests that macros define.
;;
;; lein calls run-tests after moving the :test metadata of every var its
;; selectors leave out aside, so a var that still has :test is one it
;; would run. Each is written to the listing file as one JSON line,
;; {"suite":"<namespace>","name":"<var>"}, in the order the namespaces
;; were given and, within one, by line, then name. Nothing runs, so lein
;; exits 0 unless a namespace fails to load.
(ns greenshift.lein.list
  (:require [clojure.test :as t]))

(defn- json-string
  "s as a JSON string: quote, backslash and every control character
  escaped."
  ^String [^String s]
  (let [out (StringBuilder. "\"")]
    (dotimes [i (.length s)]
      (let [c (.charAt s i)
            n (int c)]
        (cond
          (== n 0x22) (.append out "\\\"")
          (== n 0x5c) (.append out "\\\\")
          ;; controls, DEL and C1, and the line and paragraph separators
          (or (< n 0x20) (<= 0x7f n 0x9f) (== n 0x2028) (== n 0x2029))
          (.append out (format "\\u%04x" n))
          :else (.append out c))))
    (.append out "\"")
    (.toString out)))

(defn- ns-of
  "The namespace a run-tests argument names: a namespace or a symbol."
  [x]
  (if (instance? clojure.lang.Namespace x) x (the-ns x)))

(defn- tests-of
  "The vars of namespace ns that have :test metadata, by line, then name."
  [ns]
  (->> (vals (ns-interns ns))
       (filter #(:test (meta %)))
       (sort-by (fn [v] (let [m (meta v)]
                          [(or (:line m) Long/MAX_VALUE) (str (:name m))])))))

(defn install!
  "Make clojure.test/run-tests write its tests to the file at path, one
  JSON line each, and run none of them."
  [^String path]
  (alter-var-root
   #'t/run-tests
   (fn [_]
     (fn [& namespaces]
       (let [nss (distinct (map ns-of (or (seq namespaces) [*ns*])))
             lines (for [ns nss
                         v (tests-of ns)]
                     (str "{\"suite\":" (json-string (str (ns-name ns)))
                          ",\"name\":" (json-string (str (:name (meta v)))) "}\n"))]
         (spit path (apply str lines))
         {:type :summary :test 0 :pass 0 :fail 0 :error 0}))))
  :installed)
