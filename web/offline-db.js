// offline-db.js — the IndexedDB local store for the offline write sync (the
// ADR 0179 pattern, ported from the bible reader's offline-db.js to aria's
// vanilla SPA). Aria's personal state (the download queue, the playlist
// mutations) is written to the adapter over /dl/*; offline those writes must
// not be lost: they accumulate in the `synclog` store (JSON strings, key =
// op id) and are replayed in order on reconnect (the app's flushSynclog).
// `state` holds small durable read-model values the app wants to survive a
// browser restart (e.g. the last queue order).
(function () {
  "use strict";
  const DB_NAME = "aria-offline";
  const DB_VERSION = 1;
  const STORES = ["state", "synclog"];
  let dbPromise = null;

  function open() {
    if (dbPromise) return dbPromise;
    dbPromise = new Promise((resolve, reject) => {
      if (!("indexedDB" in window)) { reject(new Error("indexedDB unavailable")); return; }
      const rq = indexedDB.open(DB_NAME, DB_VERSION);
      rq.onupgradeneeded = function () {
        const db = rq.result;
        for (const name of STORES) {
          if (!db.objectStoreNames.contains(name)) db.createObjectStore(name);
        }
      };
      rq.onsuccess = function () { resolve(rq.result); };
      rq.onerror = function () { reject(rq.error); };
      rq.onblocked = function () { reject(new Error("indexedDB open blocked")); };
    });
    return dbPromise;
  }

  function withStore(store, mode, op) {
    return open().then(function (db) {
      return new Promise(function (resolve, reject) {
        const t = db.transaction(store, mode);
        const os = t.objectStore(store);
        const rq = op(os);
        t.oncomplete = function () { resolve(rq.result); };
        t.onerror = function () { reject(t.error); };
        t.onabort = function () { reject(t.error); };
      });
    });
  }

  window.AriaOfflineDB = {
    put: function (store, key, value) {
      return withStore(store, "readwrite", function (os) { return os.put(value, key); });
    },
    get: function (store, key) {
      return withStore(store, "readonly", function (os) { return os.get(key); });
    },
    del: function (store, key) {
      return withStore(store, "readwrite", function (os) { return os.delete(key); });
    },
    getAll: function (store) {
      return withStore(store, "readonly", function (os) { return os.getAll(); });
    },
    keys: function (store) {
      return withStore(store, "readonly", function (os) { return os.getAllKeys(); });
    }
  };
})();
