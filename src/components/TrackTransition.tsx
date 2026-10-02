import { AnimatePresence, motion, useIsPresent, useReducedMotion } from "motion/react";
import { Skeleton } from "@radix-ui/themes";
import { VinylRecord } from "@phosphor-icons/react";
import { AppTooltip } from "./AppTooltip";
import { textTransition, textTransitionEase } from "./textTransition";

// React Bits' Animated Content inspired the artwork direction.
// Motion owns presence, interruption and interpolation; these are app-specific transitions.
type ArtworkProps = {
  songKey: string;
  cover: string | null;
  title: string;
  hasTrack: boolean;
  loading: boolean;
  onCoverError: (cover: string) => void;
};

function ArtworkFrame({ cover, title, hasTrack, onCoverError }: ArtworkProps) {
  const present = useIsPresent();
  const reduced = useReducedMotion();
  return <motion.div
    className={`album-cover ${cover ? "has-image" : "no-image"}`}
    aria-hidden={!present} inert={!present}
    initial={reduced ? { opacity: 0 } : { opacity: 0, x: 30, y: 8, scale: 0.88, rotate: 6 }}
    animate={{ opacity: 1, x: 0, y: 0, scale: 1, rotate: 0 }}
    exit={reduced ? { opacity: 0 } : { opacity: 0, x: -24, y: -4, scale: 0.9, rotate: -6 }}
    transition={{ duration: reduced ? 0.12 : 0.52, ease: textTransitionEase }}
  >
    <AnimatePresence initial={false}>
      {cover ? <motion.img key={cover} className="artwork-layer"
        src={cover} alt={`${title}的封面`} draggable={false}
        initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}
        transition={{ duration: reduced ? 0.12 : 0.25 }}
        onError={() => onCoverError(cover)} />
        : <motion.div key="placeholder" className="cover-placeholder artwork-layer" aria-hidden="true"
          initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}
          transition={{ duration: reduced ? 0.12 : 0.2 }}>
          <div className="record-circle"><VinylRecord size={96} weight="thin" /></div>
          <span>{hasTrack ? "暂无封面" : "等待音乐"}</span>
        </motion.div>}
    </AnimatePresence>
  </motion.div>;
}

export function TrackArtwork(props: ArtworkProps) {
  return <div className="cover-stage">
    <Skeleton loading={props.loading} className="cover-skeleton">
      <div className="cover-stack">
        <AnimatePresence initial={false}>
          <ArtworkFrame key={props.songKey} {...props} />
        </AnimatePresence>
      </div>
    </Skeleton>
  </div>;
}

type MetadataProps = {
  songKey: string;
  title: string;
  artist: string;
  hasTrack: boolean;
  loading: boolean;
};

function MetadataFrame({ title, artist, hasTrack, loading }: MetadataProps) {
  const present = useIsPresent();
  const reduced = useReducedMotion();
  // Each line has its own clipping area. Old text clears it before new text enters.
  // Keeping both frames mounted lets Motion interrupt rapid skips without queuing them.
  const transition = textTransition(!!reduced);
  return <div className="track-copy" aria-hidden={!present} inert={!present}>
    <div className="title-line">
      <Skeleton loading={loading}>
        <AppTooltip content={title} className="metadata-tooltip-trigger" disabled={loading || !hasTrack || !present}>
          <motion.h1 tabIndex={hasTrack && present ? 0 : undefined}
            initial={transition.initial} exit={transition.exit} animate={transition.title}
          >{title}</motion.h1>
        </AppTooltip>
      </Skeleton>
    </div>
    <div className="artist-line">
      <Skeleton loading={loading}>
        <AppTooltip content={artist} className="metadata-tooltip-trigger" disabled={loading || !hasTrack || !present}>
        <motion.p className="artist" tabIndex={hasTrack && present ? 0 : undefined}
          initial={transition.initial} exit={transition.exit} animate={transition.artist}
        >{artist}</motion.p>
        </AppTooltip>
      </Skeleton>
    </div>
  </div>;
}

export function TrackMetadata(props: MetadataProps) {
  return <div className="song-information" aria-live="polite" aria-atomic="true">
    <AnimatePresence initial={false}>
      <MetadataFrame key={props.songKey} {...props} />
    </AnimatePresence>
  </div>;
}
