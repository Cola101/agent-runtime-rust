import { useEffect, useState } from "react";
import { connection, type Connection } from "./runtime";

/// Connection state, for anything that needs to say whether the data on screen
/// is live or a placeholder. Surfaces should say which — showing sample rows
/// as though they came from a runtime is the kind of quiet lie this project
/// spends its time removing.
export function useConnection(): Connection {
  const [state, setState] = useState<Connection>({ state: "absent" });
  useEffect(() => {
    let alive = true;
    void connection().then((next) => { if (alive) setState(next); });
    return () => { alive = false; };
  }, []);
  return state;
}
