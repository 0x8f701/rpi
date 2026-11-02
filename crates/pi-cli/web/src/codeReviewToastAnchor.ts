export const CODE_REVIEW_TOAST_GAP_PX = 8;
export const CODE_REVIEW_TOAST_TOP_VAR = '--code-review-toast-top';
export const CODE_REVIEW_TOAST_MAX_HEIGHT_VAR = '--code-review-toast-max-height';

export function codeReviewToastAnchor(input: {
  headBottom: number;
  tabBarBottom: number | null;
  summaryHeadBottom: number | null;
  composerTop: number | null;
  composerHeight: number;
  viewportHeight: number;
}): { top: number; maxHeight: number } {
  const top = Math.round(Math.max(
    input.headBottom,
    input.tabBarBottom ?? input.headBottom,
    input.summaryHeadBottom ?? input.headBottom,
  ) + CODE_REVIEW_TOAST_GAP_PX);
  const composerTop = input.composerTop;
  const gapBottom = composerTop !== null && input.composerHeight > 1
    ? composerTop - CODE_REVIEW_TOAST_GAP_PX
    : input.viewportHeight - CODE_REVIEW_TOAST_GAP_PX;
  return {
    top,
    maxHeight: Math.max(0, Math.round(gapBottom - top)),
  };
}

export function applyCodeReviewToastAnchor(
  style: CSSStyleDeclaration,
  anchor: { top: number; maxHeight: number },
): void {
  style.setProperty(CODE_REVIEW_TOAST_TOP_VAR, `${anchor.top}px`);
  style.setProperty(CODE_REVIEW_TOAST_MAX_HEIGHT_VAR, `${anchor.maxHeight}px`);
}

export function clearCodeReviewToastAnchor(style: CSSStyleDeclaration): void {
  style.removeProperty(CODE_REVIEW_TOAST_TOP_VAR);
  style.removeProperty(CODE_REVIEW_TOAST_MAX_HEIGHT_VAR);
}

export function syncCodeReviewToastAnchor(
  panel: HTMLElement,
  style: CSSStyleDeclaration,
  viewportHeight: number,
): void {
  const head = panel.querySelector('.code-review__head');
  if (head == null || typeof HTMLElement !== 'function' || !(head instanceof HTMLElement)) {
    clearCodeReviewToastAnchor(style);
    return;
  }
  const composer = panel.querySelector('.code-review__composer');
  const tabBar = panel.querySelector('.code-review__tab-bar');
  const summaryHead = panel.querySelector('.code-review-tldr__head');
  const headBox = head.getBoundingClientRect();
  const composerBox = composer instanceof HTMLElement ? composer.getBoundingClientRect() : null;
  const tabBarBox = tabBar instanceof HTMLElement ? tabBar.getBoundingClientRect() : null;
  const summaryHeadBox = summaryHead instanceof HTMLElement ? summaryHead.getBoundingClientRect() : null;
  applyCodeReviewToastAnchor(style, codeReviewToastAnchor({
    headBottom: headBox.bottom,
    tabBarBottom: tabBarBox && tabBarBox.height > 0 && tabBarBox.width > 0 ? tabBarBox.bottom : null,
    summaryHeadBottom: summaryHeadBox && summaryHeadBox.height > 0 && summaryHeadBox.width > 0 ? summaryHeadBox.bottom : null,
    composerTop: composerBox ? composerBox.top : null,
    composerHeight: composerBox ? composerBox.height : 0,
    viewportHeight,
  }));
}
