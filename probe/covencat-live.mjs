// Retrigger against the merged publisher normalization fix.
// Retrigger after remounting the temporary repair policy.
export function describeMergedProbe(value) {
  return {
    value,
    status: "broken",
  };
}
