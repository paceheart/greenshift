;; Added by `greenshift init lein`: exactly the tests named, one ns/var
;; per line, in the file -Dgreenshift.names gives (`greenshift run lein`
;; writes it). Plain `lein test` never reads it.
:greenshift (let [names (delay (->> (.split ^String (slurp (or (System/getProperty "greenshift.names")
                                                               (throw (ex-info "lein test :greenshift needs -Dgreenshift.names=FILE" {}))))
                                            "\n")
                                    (remove empty?) set))
                  nss   (delay (set (keep #(let [i (.indexOf ^String % "/")] (when (pos? i) (subs ^String % 0 i))) @names)))]
              [(fn [ns & _] (contains? @nss (str ns)))
               (fn [m & _] (contains? @names (str (ns-name (:ns m)) "/" (:name m))))])
