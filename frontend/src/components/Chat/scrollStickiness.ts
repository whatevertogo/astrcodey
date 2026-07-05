/** Within this distance from the bottom we treat the user as "following" the stream. */
export const STICK_TO_BOTTOM_THRESHOLD_PX = 80

export function isNearBottom(
  scrollTop: number,
  scrollHeight: number,
  clientHeight: number,
  threshold = STICK_TO_BOTTOM_THRESHOLD_PX
): boolean {
  return scrollHeight - scrollTop - clientHeight <= threshold
}

/**
 * Update follow-bottom stickiness from a user/programmatic scroll event.
 *
 * Only reacts when scrollTop actually changed so content growth below the
 * viewport (scrollHeight increases, scrollTop unchanged) does not disable follow.
 */
export function nextStickToBottom(
  current: boolean,
  scrollTop: number,
  lastScrollTop: number,
  nearBottom: boolean
): boolean {
  if (scrollTop === lastScrollTop) {
    return current
  }
  if (scrollTop < lastScrollTop) {
    return false
  }
  if (nearBottom) {
    return true
  }
  return current
}
