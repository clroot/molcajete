import {describe, expect, test} from 'bun:test';
import {clampTextSize, effectiveDisplay, tunnelOverrides} from './displaySettings';
import {percentOfDpi, scaleOptionsWith, SCALE_OPTIONS} from './displayScale';

const unset = {dpi: null, resizeMethod: null};
const configured = {dpi: '144', resizeMethod: 'display-update'};

describe('display settings', () => {
  test('follow the connection until this browser chooses', () => {
    expect(effectiveDisplay({}, unset)).toMatchObject({scale: 'auto', resize: false});
    expect(effectiveDisplay({}, configured)).toMatchObject({scale: 144, resize: true});
    expect(effectiveDisplay({scale: 'auto', resize: false}, configured)).toMatchObject({
      scale: 'auto',
      resize: false,
    });
    expect(effectiveDisplay({scale: 120}, unset)).toMatchObject({scale: 120, resize: false});
    expect(effectiveDisplay({}, {dpi: 'junk', resizeMethod: 'reconnect'})).toMatchObject({
      scale: 'auto',
      resize: false,
    });
  });

  test('the text size of a terminal follows the connection until this browser chooses', () => {
    expect(effectiveDisplay({}, unset).textSize).toBe(12);
    expect(effectiveDisplay({}, {...unset, fontSize: '10'}).textSize).toBe(10);
    expect(effectiveDisplay({}, {...unset, fontSize: 'big'}).textSize).toBe(12);
    expect(effectiveDisplay({textSize: 8}, {...unset, fontSize: '10'}).textSize).toBe(8);
    expect(tunnelOverrides({textSize: 8})).toEqual({'param.font-size': '8'});
    // What the session menu offers.
    expect([3, 9.4, 30].map(clampTextSize)).toEqual([6, 9, 18]);
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
