export type SheetBox = {
  left: number;
  right: number;
  top: number;
  bottom: number;
};

export const CJK_SHEET_TASK =
  '对当前代码库做一次全面审查：核对会话切换与任务描述在窄屏下完整换行，且不得覆盖最新活动与最近转录。'.repeat(3);

export function rectsIntersect(a: SheetBox, b: SheetBox): boolean {
  return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
}

export function cssBlock(css: string, selector: string): string {
  const start = css.indexOf(selector);
  if (start < 0) return '';
  const open = css.indexOf('{', start);
  const close = css.indexOf('}', open);
  if (open < 0 || close < 0) return '';
  return css.slice(open + 1, close);
}

export function subagentSheetTaskShrinks(css: string): boolean {
  const section = cssBlock(css, '.subagent-details__section {');
  const transcript = cssBlock(css, '.subagent-details__section--transcript {');
  const description = cssBlock(css, '.subagent-details__description {');
  if (/position:\s*(absolute|fixed)/.test(description)) return true;
  if (/(?:^|[^\w-])(?:max-)?height:\s*\d/.test(description)) return true;
  if (/min-height:\s*0/.test(section)) return true;
  if (/flex:\s*1\b/.test(transcript)) return true;
  return !/flex:\s*none/.test(section);
}

export function subagentSheetSectionRects(opts: {
  viewportWidth: number;
  padX: number;
  gap: number;
  statusHeight: number;
  taskText: string;
  fontSize: number;
  lineHeight: number;
  cjkAdvance: number;
  activityHeight: number;
  transcriptLabelHeight: number;
  taskShrinks: boolean;
}): { task: SheetBox; activity: SheetBox; transcript: SheetBox } {
  const inner = Math.max(1, opts.viewportWidth - 2 * opts.padX);
  const advance = opts.fontSize * opts.cjkAdvance;
  const charsPerLine = Math.max(1, Math.floor(inner / advance));
  const lines = Math.ceil([...opts.taskText].length / charsPerLine);
  const taskContentHeight = lines * opts.fontSize * opts.lineHeight;
  const taskBoxHeight = opts.taskShrinks
    ? Math.min(taskContentHeight, opts.fontSize * opts.lineHeight)
    : taskContentHeight;
  const left = opts.padX;
  const right = opts.viewportWidth - opts.padX;
  const taskTop = opts.statusHeight + opts.gap;
  const task: SheetBox = {
    left,
    right,
    top: taskTop,
    bottom: taskTop + taskContentHeight,
  };
  const activityTop = taskTop + taskBoxHeight + opts.gap;
  const activity: SheetBox = {
    left,
    right,
    top: activityTop,
    bottom: activityTop + opts.activityHeight,
  };
  const transcriptTop = activity.bottom + opts.gap;
  const transcript: SheetBox = {
    left,
    right,
    top: transcriptTop,
    bottom: transcriptTop + opts.transcriptLabelHeight,
  };
  return { task, activity, transcript };
}

export function phoneSheetRects(css: string, taskText = CJK_SHEET_TASK) {
  return subagentSheetSectionRects({
    viewportWidth: 390,
    padX: 16,
    gap: 10,
    statusHeight: 20,
    taskText,
    fontSize: 13,
    lineHeight: 1.45,
    cjkAdvance: 1,
    activityHeight: 36,
    transcriptLabelHeight: 16,
    taskShrinks: subagentSheetTaskShrinks(css),
  });
}
