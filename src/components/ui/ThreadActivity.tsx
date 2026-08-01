import { useTranslation } from "react-i18next";
import type { ThreadActivityView } from "../../state/threadActivity";
import { cn } from "../../lib/cn";
import {
  threadActivityMarkers,
  type ThreadActivityMarker,
  type ThreadActivityTone,
} from "./threadActivityMarkers";

const ACTIVITY_STYLES: Record<ThreadActivityTone, { text: string; dot: string }> = {
  running: {
    text: "text-running",
    dot: "weft-pulse h-1.5 w-1.5 rounded-full bg-running",
  },
};

const ACTIVITY_TITLE_KEYS: Record<ThreadActivityTone, "workspace.live"> = {
  running: "workspace.live",
};

function ActivityCount({
  marker,
  title,
}: {
  marker: ThreadActivityMarker;
  title: string;
}) {
  const style = ACTIVITY_STYLES[marker.kind];
  return (
    <span className={cn("inline-flex items-center gap-1", style.text)} title={title}>
      <span className={style.dot} />
      {marker.count}
    </span>
  );
}

/** Compact, shared presentation for a thread's canonical activity view. */
export function ThreadActivity({
  activity,
  className,
}: {
  activity: ThreadActivityView;
  className?: string;
}) {
  const { t } = useTranslation();
  const markers = threadActivityMarkers(activity);

  if (markers.length === 0) return null;

  return (
    <span className={cn("inline-flex items-center gap-1.5 tabular-nums", className)}>
      {markers.map((marker) => (
        <ActivityCount
          key={marker.kind}
          marker={marker}
          title={t(ACTIVITY_TITLE_KEYS[marker.kind], { count: marker.count })}
        />
      ))}
    </span>
  );
}
