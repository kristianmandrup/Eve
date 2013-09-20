(ns aurora.transformers.editor
  (:require [dommy.core :as dommy]
            [aurora.util.xhr :as xhr]
            [aurora.util.async :as async]
            [clojure.walk :as walk]
            [cljs.reader :as reader]
            [cljs.core.async.impl.protocols :as protos]
            [cljs.core.async :refer [put! chan sliding-buffer take! timeout]])
  (:require-macros [dommy.macros :refer [node sel sel1]]
                   [cljs.core.async.macros :refer [go]]))

(defn instrument-pipes [prog]
  (assoc prog :pipes
    (into []
          (for [pipe (:pipes prog)]
            (assoc pipe :pipe
              (reduce (fn [cur v]
                        (conj cur v (list 'js/aurora.transformers.editor.step (list 'quote (:name pipe)) '_PREV_))
                        )
                      [(list 'js/aurora.transformers.editor.scope (list 'quote (:name pipe)) (list 'zipmap (list 'quote (:scope pipe)) (:scope pipe)))]
                      (:pipe pipe)))))))

(def captures (js-obj))

(def allow-commute true)
(def current-pipe nil)

(defn scope [name scope]
  (when-not (aget captures (str name))
    (aset captures (str name) (array)))
  (.push (aget captures (str name)) (js-obj "scope" scope "steps" (array))))

(defn step [name v]
  (.push (-> (aget captures (str name)) last (aget "steps")) v)
  v)

(defn !runner [prog full? pipe]
  (go
   (while (<! listener-loop)
     (println "here")
     (js/aurora.engine.commute (assoc js/aurora.pipelines.state "dirty" false))
     ;(put! js/aurora.engine.event-loop :sub-commute)
     ))
  (println "current-pipe" pipe)
  (set! current-pipe pipe)
  (exec-program (instrument-pipes prog) full?))


(defn !in-running [thing]
  (when js/running.pipelines
    (aget js/running.pipelines thing)))

(defn meta-walk [cur path]
  (when (and (not= nil cur)
             (satisfies? IMeta cur))
    (alter-meta! cur cljs.core/assoc :run-path path)
    (cond
     (or (list? cur) (seq? cur)) (doseq [[k v] (map-indexed vector cur)]
                                   (meta-walk v (cljs.core/conj path k)))
     (map? cur) (doseq [[k v] cur]
                  (meta-walk v (cljs.core/conj path k)))
     (vector? cur) (doseq [[k v] (map-indexed vector cur)]
                     (meta-walk v (cljs.core/conj path k)))))
  cur)

(defn ->step [name step iter]
  (let [get-i (if iter
                #(nth % iter)
                last)]
    (when-let [cap (-> js/aurora.transformers.editor.captures
									 (aget (str name))
									 (get-i))]
      (when (aget cap "steps")
        (-> cap (aget "steps") (aget step) )))))

(defn ->scope [name iter]
  (let [get-i (if iter
                #(nth % iter)
                last)]
    (when-let [cap (-> js/aurora.transformers.editor.captures
									 (aget (str name))
									 (get-i))]
      (when (seq (aget cap "scope"))
        (zipmap (map (fn [x]
                       (-> x str symbol))
                     (keys (aget cap "scope")))
                (vals (aget cap "scope")))))))

(defn commute [v]
  (when allow-commute
    (let [path (-> v meta :run-path)
          v (if (seq? v)
              (with-meta (vec v) (meta v))
              v)]
      (aset js/running.pipelines (first path) (if (next path)
                                                (assoc-in (aget js/running.pipelines (first path)) (rest path) v)
                                                v))

      (println "[running] commute: " v path)
      (meta-walk v path)
      (put! event-loop :commute))))

(defn run-special [pipe]
  (let [func (aget js/running.pipelines (str (:name pipe)))
        scope (if-let [s (->scope (:name pipe))]
                (map s (:scope pipe))
                (map #(aget js/running.pipelines (str %)) (:scope pipe)))]
    (println "running special!")
    (set! allow-commute false)
    (when func
      (apply func scope))
    (set! allow-commute true)
    (set! current-pipe nil)))

(set! js/running (js-obj))

(def listener-loop (chan))
(def event-loop (chan))

(defn start-main-loop [main]
  (let [debounced (async/debounce event-loop 1)]
  (go
   (loop [run? true]
     (when run?
       (.time js/console "[child] runtime")
       (main)
       (when current-pipe
         (run-special current-pipe))
       (.timeEnd js/console "[child] runtime")
       (put! listener-loop :done)
       (recur (<! debounced)))))))

(defn exec-program [prog clear? pipe]
  (when (or clear? (not js/running.pipelines))
    (set! js/running.pipelines (js-obj)))
  (doseq [[k v] (:data prog)
          :when (not (aget js/running.pipelines (str k)))
          :let [v (reader/read-string (pr-str v))]]
    (meta-walk v [k])
    (aset js/running.pipelines (str k) v))
  (put! event-loop false)
  (set! js/aurora.transformers.editor.event-loop (chan))
  (put! listener-loop false)
  (set! js/aurora.transformers.editor.listener-loop (chan))
  (go
   (let [pipes (<! (xhr/xhr [:post "http://localhost:8082/code"] {:code (pr-str (:pipes prog))
                                                                  :ns-prefix "running"}))]
     (.eval js/window pipes)
     (println "evaled: " (subs pipes 0 10))
     (start-main-loop (fn []
                        (let [main-fn (aget js/running.pipelines (str (:main prog)))
                              main-pipe (first (filter #(= (:main prog) (:name %)) (:pipes prog)))
                              vals (map #(aget js/running.pipelines (str %)) (:scope main-pipe))]
                          (println "calling main with: " vals)
                          (apply main-fn vals))))

     )))
