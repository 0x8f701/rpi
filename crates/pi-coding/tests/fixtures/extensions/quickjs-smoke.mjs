// Phase 1 QuickJS smoke fixture: an interrupt-bound spin, a memory-bound
// allocation, and a light probe used to prove the runtime stays usable after
// each guard fires.
export default function (pi) {
  pi.registerCommand("spin", {
    handler: () => {
      while (true) {}
    },
  });
  pi.registerCommand("allocate", {
    handler: () => {
      const a = new Array(1 << 26);
      a.fill(1);
      return a.length;
    },
  });
  pi.registerCommand("probe", {
    handler: () => "probe-ok",
  });
}
