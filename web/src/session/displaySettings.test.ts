import {describe, expect, test} from 'bun:test';
import {effectiveDisplay, tunnelOverrides} from './displaySettings';
import {percentOfDpi, scaleOptionsWith, SCALE_OPTIONS} from './displayScale';

const unset = {dpi: null, resizeMethod: null};
const configured = {dpi: '144', resizeMethod: 'display-update'};

describe('display settings', () => {
  test('follow the connection until this browser chooses', () => {
    expect(effectiveDisplay({}, unset)).toEqual({scale: 'auto', resize: false});
    expect(effectiveDisplay({}, configured)).toEqual({scale: 144, resize: true});
    expect(effectiveDisplay({scale: 'auto', resize: false}, configured)).toEqual({
      scale: 'auto',
      resize: false,
    });
    expect(effectiveDisplay({scale: 120}, unset)).toEqual({scale: 120, resize: false});
    expect(effectiveDisplay({}, {dpi: 'junk', resizeMethod: 'reconnect'})).toEqual({
      scale: 'auto',
      resize: false,
    });
  });

  test('send only what this browser chose', () => {
    expect(tunnelOverrides({})).toEqual({});
    expect(tunnelOverrides({scale: 168, resize: true})).toEqual({
      'param.dpi': '168',
      'param.resize-method': 'display-update',
    });
    // Empty values remove the connection's own.
    expect(tunnelOverrides({scale: 'auto', resize: false})).toEqual({
      'param.dpi': '',
      'param.resize-method': '',
    });
  });

  test('scales map to the DPI the daemons expect', () => {
    expect(SCALE_OPTIONS.map(o => [o.label, o.value])).toEqual([
      ['100 %', '96'],
      ['125 %', '120'],
      ['150 %', '144'],
      ['175 %', '168'],
      ['200 %', '192'],
      ['250 %', '240'],
      ['300 %', '288'],
    ]);
    expect(percentOfDpi(144)).toBe(150);
    expect(scaleOptionsWith('144')).toBe(SCALE_OPTIONS);
    expect(scaleOptionsWith('110').at(-1)).toEqual({value: '110', label: '115 %'});
    expect(scaleOptionsWith('x')).toBe(SCALE_OPTIONS);
  });
});
