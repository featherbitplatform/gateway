import { describe, expect, it } from 'vitest';
import {
  type ConditionGroup,
  emptyExpr,
  fromExpr,
  fromVarsList,
  opsFor,
  toExpr,
  toVarsList,
} from './conditions';

const rule = (over: Partial<import('./conditions').ConditionRule>) => ({
  kind: 'rule' as const,
  subject: 'header' as const,
  name: '',
  negate: false,
  op: '==',
  value: '',
  valueType: 'string' as const,
  values: [],
  ...over,
});

describe('conditions serialization', () => {
  it('serializes header/query/cookie/var/jsonpath subjects', () => {
    const root: ConditionGroup = {
      ...emptyExpr(),
      children: [
        rule({ subject: 'header', name: 'X-Api-Key', op: 'present' }),
        rule({ subject: 'query', name: 'page', op: '>', value: '1', valueType: 'number' }),
        rule({ subject: 'cookie', name: 'session', op: 'present' }),
        rule({ subject: 'var', name: 'remote_addr', op: 'ipmatch', values: ['10.0.0.0/8'] }),
        rule({ subject: 'jsonpath-request', name: '$.user.id', op: 'is_null', negate: true }),
        rule({ subject: 'jsonpath-response', name: '$.ok', op: '==', value: 'true', valueType: 'boolean' }),
      ],
    };
    expect(toExpr(root)).toEqual([
      ['http_x_api_key', 'present'],
      ['arg_page', '>', 1],
      ['cookie_session', 'present'],
      ['remote_addr', 'ipmatch', ['10.0.0.0/8']],
      ['$.user.id', '!', 'is_null'],
      ['response_body:$.ok', '==', true],
    ]);
  });

  it('round-trips builder models', () => {
    const root: ConditionGroup = {
      ...emptyExpr(),
      children: [
        rule({ subject: 'header', name: 'authorization', op: 'contains', value: 'Bearer' }),
        {
          kind: 'group',
          logic: 'OR',
          negate: false,
          children: [
            rule({ subject: 'jsonpath-request', name: '$.user.email', op: 'present' }),
            {
              kind: 'group',
              logic: 'AND',
              negate: true,
              children: [rule({ subject: 'jsonpath-request', name: '$.user.id', op: 'is_null' })],
            },
          ],
        },
      ],
    };
    expect(fromExpr(toExpr(root))).toEqual(root);
  });

  it('round-trips hand-authored expressions', () => {
    const exprs: unknown[] = [
      [['http_authorization', 'present']],
      [['arg_name', '==', 'jack'], ['OR', ['$.a', 'present'], ['NOT', ['$.b', 'is_null']]]],
      [['$.items[*].price', '!', '<=', 0]],
      [['remote_addr', 'ipmatch', ['10.0.0.0/8', '192.168.1.1']]],
    ];
    for (const e of exprs) {
      const model = fromExpr(e);
      expect(model, JSON.stringify(e)).not.toBeNull();
      expect(toExpr(model!)).toEqual(e);
    }
  });

  it('returns null for unrepresentable expressions', () => {
    expect(fromExpr('not an array')).toBeNull();
    expect(fromExpr([['http_a', '==', { nested: 'object' }]])).toBeNull();
    expect(fromExpr([['NOT']])).toBeNull();
    expect(fromExpr([[42, '==', 'x']])).toBeNull();
  });

  it('maps vars lists to an OR root of AND groups and back', () => {
    const varsList = [
      [['arg_name', '==', 'jack'], ['http_x', 'present']],
      [['arg_name', '==', 'rose']],
    ];
    const model = fromVarsList(varsList);
    expect(model).not.toBeNull();
    expect(model!.logic).toBe('OR');
    expect(model!.children).toHaveLength(2);
    expect(toVarsList(model!)).toEqual(varsList);
  });

  it('filters operators by subject kind', () => {
    expect(opsFor('header')).not.toContain('is_null');
    expect(opsFor('jsonpath-request')).toContain('is_null');
    expect(opsFor('var')).toContain('ipmatch');
  });
});
