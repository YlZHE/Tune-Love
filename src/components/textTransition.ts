export const textTransitionEase = [0.22, 1, 0.36, 1] as const;

export function textTransition(reduced: boolean) {
  const initial = { opacity: 0, y: reduced ? "0%" : "110%" };
  const exit = {
    opacity: 0,
    y: reduced ? "0%" : "-110%",
    transition: { duration: reduced ? 0.1 : 0.12, ease: textTransitionEase, delay: 0 },
  };
  return {
    initial,
    exit,
    title: {
      opacity: 1,
      y: "0%",
      transition: { duration: reduced ? 0.12 : 0.34, delay: reduced ? 0 : 0.14, ease: textTransitionEase },
    },
    artist: {
      opacity: 1,
      y: "0%",
      transition: { duration: reduced ? 0.12 : 0.32, delay: reduced ? 0 : 0.2, ease: textTransitionEase },
    },
  };
}
