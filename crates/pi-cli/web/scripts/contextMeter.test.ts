#!/usr/bin/env node
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import {
  beginContextUsageRequest,
  contextMeterAccessible,
  contextMeterMobile,
  contextMeterTotalK,
  formatContextMeter,
  ownsContextUsageResponse,
  parseContextUsage,
  sessionStatsCommand,
  type ContextUsageWire,
} from '../src/contextMeter.ts';
import { routeCommandSession } from '../src/goal.ts';


const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// --- parseContextUsage -----------------------------------------------------

{
  // Canonical backend wire: get_session_stats -> SessionStats.contextUsage
  // (serde camelCase; tokens/percent nullable).
  const full = parseContextUsage({ contextUsage: { tokens: 84_000, contextWindow: 200_000, percent: 42.4 } });
  check('full wire parses', JSON.stringify(full) === JSON.stringify({ tokens: 84_000, contextWindow: 200_000, percent: 42.4 } as ContextUsageWire));
  check('null contextUsage block', parseContextUsage({}) === null);
  check('null contextUsage value', parseContextUsage({ contextUsage: null }) === null);
  check('non-object contextUsage', parseContextUsage({ contextUsage: 'big' }) === null);
  check('missing data is null', parseContextUsage(null) === null);
  check('number data is null', parseContextUsage(42) === null);
  check('zero context window renders nothing', parseContextUsage({ contextUsage: { tokens: 10, contextWindow: 0, percent: 1 } }) === null);
  check('negative context window renders nothing', parseContextUsage({ contextUsage: { tokens: 10, contextWindow: -5, percent: 1 } }) === null);
  check('missing context window renders nothing', parseContextUsage({ contextUsage: { tokens: 10 } }) === null);
  const tokensNull = parseContextUsage({ contextUsage: { tokens: null, contextWindow: 370000, percent: null } });
  check('legitimate null tokens survive', !!tokensNull && tokensNull.tokens === null && tokensNull.percent === null);
  check('absent tokens survives as null', !!parseContextUsage({ contextUsage: { contextWindow: 370000 } }) && parseContextUsage({ contextUsage: { contextWindow: 370000 } })!.tokens === null);
  const hostile = parseContextUsage({ contextUsage: { tokens: Number.NaN, contextWindow: Number.POSITIVE_INFINITY, percent: Number.NaN } });
  check('NaN/Infinity window renders nothing', hostile === null);
  const hostileTokens = parseContextUsage({ contextUsage: { tokens: Number.NaN, contextWindow: 1000, percent: Number.NaN } });
  check('NaN tokens clamps to null', !!hostileTokens && hostileTokens.tokens === null);
  check('NaN percent clamps to null', !!hostileTokens && hostileTokens.percent === null);
  const fractional = parseContextUsage({ contextUsage: { tokens: 12.9, contextWindow: 1000.7, percent: -3 } });
  check('fractional values floor/clamp', JSON.stringify(fractional) === JSON.stringify({ tokens: 12, contextWindow: 1000, percent: -3 } as ContextUsageWire));
}

// --- formatContextMeter: legacy text form RETIRED (desktop unification) ----
// ALL breakpoints render contextMeterMobile's bar+percent+total; the text
// form is no longer a rendered surface. formatContextMeter stays exported
// (kept for any legacy callers) but the SSR harness below pins the BAR form.

{
  check('legacy formatter still parses (unused surface)', formatContextMeter({ tokens: 84_000, contextWindow: 200_000, percent: 42.0 }) === '84k/200k');
}

// --- contextMeterMobile / contextMeterTotalK (≤720px bar + percent + total) --

{
  check('mobile null renders nothing', contextMeterMobile(null) === null);
  check('mobile zero window renders nothing', contextMeterMobile({ tokens: 10, contextWindow: 0, percent: 1 }) === null);
  check('mobile parts for a 1M window', (() => {
    const parts = contextMeterMobile({ tokens: 620_000, contextWindow: 1_000_000, percent: 62 });
    return parts !== null && parts.percent === 62 && parts.total === '1M';
  })());
  check('mobile total abbreviates past a million (1M, not 1000k)',
    contextMeterTotalK(1_000_000) === '1M' && contextMeterTotalK(2_500_000) === '2M');
  check('mobile total keeps tokenK below a million',
    contextMeterTotalK(200_000) === '200k' && contextMeterTotalK(999) === '999');
  check('mobile percent rounds and clamps to [0, 100]',
    contextMeterMobile({ tokens: 1, contextWindow: 1000, percent: -10 })?.percent === 0
      && contextMeterMobile({ tokens: 10_000, contextWindow: 1000, percent: 99999 })?.percent === 100
      && contextMeterMobile({ tokens: 512, contextWindow: 4096, percent: 12.5 })?.percent === 13);
  check('mobile missing percent defaults to 0', contextMeterMobile({ tokens: 0, contextWindow: 370_000, percent: null })?.percent === 0);
}

// --- contextMeterAccessible (aria/title: percent + exact raw values) --------

{
  check('null renders empty', contextMeterAccessible(null) === '');
  check('zero window renders empty', contextMeterAccessible({ tokens: 10, contextWindow: 0, percent: 1 }) === '');
  check('canonical accessible label', contextMeterAccessible({ tokens: 84_000, contextWindow: 200_000, percent: 42.0 }) === '42% 84000/200000');
  check('rounds percent', contextMeterAccessible({ tokens: 512, contextWindow: 4096, percent: 12.5 }) === '13% 512/4096');
  check('window below a thousand stays raw', contextMeterAccessible({ tokens: 512, contextWindow: 999, percent: 50 }) === '50% 512/999');
  check('missing percent defaults to 0', contextMeterAccessible({ tokens: 0, contextWindow: 370000, percent: null }) === '0% 0/370000');
  check('missing tokens defaults to 0', contextMeterAccessible({ tokens: null, contextWindow: 370000, percent: 5 }) === '5% 0/370000');
  check('negative percent clamps to 0', contextMeterAccessible({ tokens: 1, contextWindow: 1000, percent: -10 }) === '0% 1/1000');
  check('huge percent clamps to 999', contextMeterAccessible({ tokens: 10_000_000, contextWindow: 10_000, percent: 99999 }) === '999% 10000000/10000');
  check('fractional percent rounds', contextMeterAccessible({ tokens: 84_000, contextWindow: 200_000, percent: 42.4 }) === '42% 84000/200000');
}

{
  // Mirrors App's #composer-context render: the BAR form on ALL breakpoints
  // (desktop unification) — bar fill width = percent, tiny percent label,
  // M-abbreviated total; the accessible name/title keep the full numbers.
  const span = (usage: ContextUsageWire | null) => {
    const parts = contextMeterMobile(usage);
    return renderToStaticMarkup(
      createElement('span', {
        id: 'composer-context',
        role: 'status',
        'aria-label': contextMeterAccessible(usage)
          ? `Context window usage: ${contextMeterAccessible(usage)}`
          : 'Context window usage',
        title: contextMeterAccessible(usage)
          ? `Context window usage: ${contextMeterAccessible(usage)}`
          : 'Context window usage',
        children: parts !== null && [
          createElement('span', {
            key: 'bar',
            className: `composer-context__bar${parts.percent >= 90 ? ' composer-context__bar--hot' : parts.percent >= 70 ? ' composer-context__bar--warn' : ''}`,
            'aria-hidden': 'true',
          }, createElement('span', { className: 'composer-context__fill', style: { width: `${parts.percent}%` } })),
          createElement('span', { key: 'percent', className: 'composer-context__percent' }, `${parts.percent}%`),
          createElement('span', { key: 'total', className: 'composer-context__total' }, parts.total),
        ],
      }),
    );
  };

  const live = span({ tokens: 620_000, contextWindow: 1_000_000, percent: 62 });
  check('SSR renders the bar form (no legacy text)', !live.includes('620k/1000k'), live);
  check('SSR fill width tracks the percent', live.includes('width:62%'), live);
  check('SSR percent label renders', live.includes('>62%</span>'), live);
  check('SSR total abbreviates to 1M', live.includes('>1M</span>'), live);
  check('SSR aria-label carries percent and exact raw values', live.includes('aria-label="Context window usage: 62% 620000/1000000"'), live);
  check('SSR title carries percent and exact raw values', live.includes('title="Context window usage: 62% 620000/1000000"'), live);
  check('SSR mid threshold keeps the default accent bar', !live.includes('composer-context__bar--warn') && !live.includes('composer-context__bar--hot'), live);
  const warn = span({ tokens: 150_000, contextWindow: 200_000, percent: 75 });
  check('warn threshold (≥70%) colors the fill', warn.includes('composer-context__bar--warn'), warn);
  const hot = span({ tokens: 95_000, contextWindow: 100_000, percent: 95 });
  check('hot threshold (≥90%) colors the fill', hot.includes('composer-context__bar--hot'), hot);

  const hidden = span(null);
  check('empty meter renders no bar parts', hidden.includes('></span>'), hidden);
  check('SSR empty meter keeps plain accessible name', hidden.includes('aria-label="Context window usage"'), hidden);
  check('SSR empty meter keeps plain title', hidden.includes('title="Context window usage"'), hidden);
}

{
  check('sessionStatsCommand type', sessionStatsCommand('A').type === 'get_session_stats');
  check('sessionStatsCommand owns session A', sessionStatsCommand('A').sessionId === 'A');
  check('stats for A routes to A while B is active', routeCommandSession(sessionStatsCommand('A'), 'B') === 'A');
  check('stats for B routes to B while A is active', routeCommandSession(sessionStatsCommand('B'), 'A') === 'B');
  check('bare get_session_stats would route to active (the bug)', routeCommandSession({ type: 'get_session_stats' }, 'B') === 'B');
  check('sessionStatsCommand does not carry auxiliary; App stamps it at send', !('auxiliary' in sessionStatsCommand('A')));
}

{
  const first = beginContextUsageRequest({}, 'A');
  const second = beginContextUsageRequest(first.bySessionId, 'A');
  const other = beginContextUsageRequest(second.bySessionId, 'B');
  check('context request sequence increments per session', first.sequence === 1 && second.sequence === 2);
  check('newer same-session response owns the cache', ownsContextUsageResponse(other.bySessionId, 'A', second.sequence));
  check('older same-session response is stale', !ownsContextUsageResponse(other.bySessionId, 'A', first.sequence));
  check('other session sequence stays independent', ownsContextUsageResponse(other.bySessionId, 'B', other.sequence));
}


console.log(`contextMeter.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`FAIL ${failure}`);
  process.exit(1);
}