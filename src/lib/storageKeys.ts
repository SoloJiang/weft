/** Central registry for all frontend localStorage keys. Keeps keys from
 *  scattering across components and makes collisions/renames easy to audit. */
export const STORAGE_KEYS = {
  // i18n
  lang: "weft-lang",
  // theme
  theme: "weft-theme",
  // onboarding
  onboardingDismissed: "weft-first-run-onboarding-v2-dismissed",
  // settings
  projectsDir: "weft-projects-dir",
  reviewSkill: "weft-review-skill",
  autoReview: "weft-auto-review",
  notify: "weft-notify",
  keepAwake: "weft-keep-awake",
  dangerousMode: "weft-dangerous",
  idleCapMins: "weft-idle-cap-mins",
  wallCapMins: "weft-wall-cap-mins",
  // navigation / session
  activeWorkspace: "weft-active-workspace",
  // nudges
  dangerNudge: "weft-danger-nudge",
  // panel widths
  repoSidePanelWidth: "weft-repopanel-w",
  diffPanelWidth: "weft-diff-w",
  filesPanelWidth: "weft-files-w",
  testPlanPanelWidth: "weft-testplan-w",
} as const;
