/* Icon set — stroke-based, currentColor. Plus the weft "weave mark". */
const S = ({ children, size = 16, sw = 1.75, fill = "none", style }) => (
  <svg width={size} height={size} viewBox="0 0 24 24" fill={fill} stroke="currentColor"
       strokeWidth={sw} strokeLinecap="round" strokeLinejoin="round" style={style} aria-hidden="true">
    {children}
  </svg>
);

/* ---- the weft weave mark: three warp lines converging to one weft dot ---- */
const WeaveMark = ({ size = 22, animate = false }) => (
  <svg width={size} height={size} viewBox="0 0 32 32" fill="none" aria-hidden="true"
       className={animate ? "weave-spin" : ""}>
    <path d="M3 8 H20" stroke="var(--warp)" strokeWidth="2" strokeLinecap="round" opacity="0.9" />
    <path d="M3 16 H24" stroke="var(--warp)" strokeWidth="2" strokeLinecap="round" opacity="0.7" />
    <path d="M3 24 H20" stroke="var(--warp)" strokeWidth="2" strokeLinecap="round" opacity="0.5" />
    <path d="M20 8 Q27 8 27 16 Q27 24 20 24" stroke="var(--weft)" strokeWidth="2" strokeLinecap="round" fill="none" />
    <circle cx="27" cy="16" r="2.6" fill="var(--weft)" />
  </svg>
);

const IconHome    = (p) => <S {...p}><path d="M4 19V9.5a2 2 0 0 1 .9-1.67l6-4a2 2 0 0 1 2.2 0l6 4A2 2 0 0 1 20 9.5V19a1 1 0 0 1-1 1h-4v-6H9v6H5a1 1 0 0 1-1-1Z"/></S>;
const IconBoard   = (p) => <S {...p}><rect x="3" y="4" width="5" height="16" rx="1.3"/><rect x="9.5" y="4" width="5" height="10" rx="1.3"/><rect x="16" y="4" width="5" height="13" rx="1.3"/></S>;
const IconRepos   = (p) => <S {...p}><circle cx="6" cy="6" r="2.4"/><circle cx="18" cy="7" r="2.4"/><circle cx="12" cy="18" r="2.4"/><path d="M7.7 7.6 10.5 16M16.6 9 13 16.4M8.2 6.4h7.3"/></S>;
const IconThread  = (p) => <S {...p}><path d="M3 7h18M3 12h18M3 17h11"/></S>;
const IconBell    = (p) => <S {...p}><path d="M6 8a6 6 0 0 1 12 0c0 5 2 6 2 6H4s2-1 2-6Z"/><path d="M10 19a2 2 0 0 0 4 0"/></S>;
const IconBolt    = (p) => <S {...p}><path d="M13 2 4 14h7l-1 8 9-12h-7l1-8Z"/></S>;
const IconSpark   = (p) => <S {...p}><path d="M12 3v4M12 17v4M3 12h4M17 12h4M6.3 6.3 9 9M15 15l2.7 2.7M17.7 6.3 15 9M9 15l-2.7 2.7"/></S>;
const IconArrow   = (p) => <S {...p}><path d="M5 12h14M13 6l6 6-6 6"/></S>;
const IconChevR   = (p) => <S {...p}><path d="M9 6l6 6-6 6"/></S>;
const IconChevD   = (p) => <S {...p}><path d="M6 9l6 6 6-6"/></S>;
const IconCheck   = (p) => <S {...p}><path d="M5 12.5 10 17l9-10"/></S>;
const IconCheck2  = (p) => <S {...p}><path d="M2 13l4 4 8-9M11 16l1 1 8-9"/></S>;
const IconX       = (p) => <S {...p}><path d="M6 6l12 12M18 6 6 18"/></S>;
const IconShield  = (p) => <S {...p}><path d="M12 3 5 6v5c0 4.5 3 7.5 7 9 4-1.5 7-4.5 7-9V6l-7-3Z"/><path d="M9.5 12 11.5 14 15 10"/></S>;
const IconShieldQ = (p) => <S {...p}><path d="M12 3 5 6v5c0 4.5 3 7.5 7 9 4-1.5 7-4.5 7-9V6l-7-3Z"/><path d="M10.5 9.5a1.6 1.6 0 1 1 2.4 1.4c-.6.4-.9.7-.9 1.4M12 15.5v.01"/></S>;
const IconBranch  = (p) => <S {...p}><circle cx="6" cy="6" r="2.2"/><circle cx="6" cy="18" r="2.2"/><circle cx="18" cy="8" r="2.2"/><path d="M6 8.2v7.6M8.2 6.4H14a2 2 0 0 1 2 2v1.4"/></S>;
const IconMerge   = (p) => <S {...p}><circle cx="6" cy="6" r="2.2"/><circle cx="6" cy="18" r="2.2"/><circle cx="18" cy="15" r="2.2"/><path d="M6 8.2v7.6M8.1 6.6c1.2 5 4 6.6 7.7 7.2"/></S>;
const IconTerminal= (p) => <S {...p}><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 9l3 3-3 3M13 15h4"/></S>;
const IconFile    = (p) => <S {...p}><path d="M6 3h7l5 5v13a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1Z"/><path d="M13 3v5h5"/></S>;
const IconPlus    = (p) => <S {...p}><path d="M12 5v14M5 12h14"/></S>;
const IconSearch  = (p) => <S {...p}><circle cx="11" cy="11" r="6.5"/><path d="M20 20l-3.8-3.8"/></S>;
const IconSun     = (p) => <S {...p}><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4 12H2M22 12h-2M5 5 3.6 3.6M20.4 20.4 19 19M19 5l1.4-1.4M3.6 20.4 5 19"/></S>;
const IconMoon    = (p) => <S {...p}><path d="M20 14.5A8 8 0 1 1 9.5 4 6.5 6.5 0 0 0 20 14.5Z"/></S>;
const IconSend    = (p) => <S {...p}><path d="M5 12 20 5l-5 15-3.5-6L5 12Z"/></S>;
const IconRadio   = (p) => <S {...p}><circle cx="12" cy="12" r="2.2"/><path d="M7.5 7.5a6.4 6.4 0 0 0 0 9M16.5 7.5a6.4 6.4 0 0 1 0 9M4.8 4.8a10 10 0 0 0 0 14.4M19.2 4.8a10 10 0 0 1 0 14.4"/></S>;
const IconClock   = (p) => <S {...p}><circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/></S>;
const IconEye     = (p) => <S {...p}><path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12Z"/><circle cx="12" cy="12" r="2.6"/></S>;
const IconPencil  = (p) => <S {...p}><path d="M14 5l5 5M4 20l1-4L16 5l3 3L8 19l-4 1Z"/></S>;
const IconBan     = (p) => <S {...p}><circle cx="12" cy="12" r="8.5"/><path d="M6 6l12 12"/></S>;
const IconBox     = (p) => <S {...p}><path d="M12 3 3.5 7.5v9L12 21l8.5-4.5v-9L12 3Z"/><path d="M3.7 7.7 12 12l8.3-4.3M12 21v-9"/></S>;
const IconFolder  = (p) => <S {...p}><path d="M3 7a1 1 0 0 1 1-1h5l2 2h8a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V7Z"/></S>;
const IconCopy    = (p) => <S {...p}><rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1"/></S>;
const IconExternal= (p) => <S {...p}><path d="M14 4h6v6M20 4l-9 9M18 13v5a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/></S>;
const IconSettings= (p) => <S {...p}><circle cx="12" cy="12" r="3"/><path d="M19 12a7 7 0 0 0-.1-1.3l2-1.5-2-3.4-2.3 1a7 7 0 0 0-2.2-1.3L14 2h-4l-.4 2.2a7 7 0 0 0-2.2 1.3l-2.3-1-2 3.4 2 1.5A7 7 0 0 0 5 12a7 7 0 0 0 .1 1.3l-2 1.5 2 3.4 2.3-1a7 7 0 0 0 2.2 1.3L10 22h4l.4-2.2a7 7 0 0 0 2.2-1.3l2.3 1 2-3.4-2-1.5A7 7 0 0 0 19 12Z"/></S>;
const IconLayers  = (p) => <S {...p}><path d="M12 3 3 7.5l9 4.5 9-4.5L12 3Z"/><path d="M3 12l9 4.5L21 12M3 16.5 12 21l9-4.5"/></S>;
const IconDot     = (p) => <S {...p} fill="currentColor" sw="0"><circle cx="12" cy="12" r="3.5"/></S>;
const IconWarn    = (p) => <S {...p}><path d="M12 4 2.5 20h19L12 4Z"/><path d="M12 10v4M12 17v.01"/></S>;
const IconReplay  = (p) => <S {...p}><path d="M4 12a8 8 0 1 0 2.5-5.8M4 4v3.5h3.5"/></S>;
const IconFlow    = (p) => <S {...p}><rect x="3" y="4" width="6" height="5" rx="1.3"/><rect x="15" y="4" width="6" height="5" rx="1.3"/><rect x="9" y="15" width="6" height="5" rx="1.3"/><path d="M6 9v2a2 2 0 0 0 2 2h1M18 9v2a2 2 0 0 1-2 2h-1"/></S>;
const IconSidebar = (p) => <S {...p}><rect x="3" y="4.5" width="18" height="15" rx="2"/><path d="M9 4.5v15"/></S>;

Object.assign(window, {
  S, WeaveMark,
  IconHome, IconBoard, IconRepos, IconThread, IconBell, IconBolt, IconSpark, IconArrow,
  IconChevR, IconChevD, IconCheck, IconCheck2, IconX, IconShield, IconShieldQ, IconBranch,
  IconMerge, IconTerminal, IconFile, IconPlus, IconSearch, IconSun, IconMoon, IconSend,
  IconRadio, IconClock, IconEye, IconPencil, IconBan, IconBox, IconFolder, IconCopy,
  IconExternal, IconSettings, IconLayers, IconDot, IconWarn, IconReplay, IconFlow, IconSidebar,
});
