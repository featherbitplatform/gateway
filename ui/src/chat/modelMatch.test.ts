import { describe, expect, it } from 'vitest';
import { rankModels, scoreModel } from './modelMatch';

const IDS = ['gpt-5', 'gpt-5-mini', 'gpt-4.1', 'gpt-4.1-mini', 'o3', 'o4-mini', 'text-embedding-3-small'];

describe('scoreModel', () => {
  it('ranks exact > prefix > substring > subsequence > none, case-insensitively', () => {
    expect(scoreModel('gpt-5', 'gpt-5')).toBe(100);
    expect(scoreModel('GPT-5', 'gpt-5-mini')).toBe(80);
    expect(scoreModel('mini', 'gpt-5-mini')).toBe(60);
    expect(scoreModel('g5m', 'gpt-5-mini')).toBe(30);
    expect(scoreModel('claude', 'gpt-5-mini')).toBe(0);
  });
  it('treats an empty or blank query as a weak match for everything', () => {
    expect(scoreModel('', 'o3')).toBe(1);
    expect(scoreModel('   ', 'o3')).toBe(1);
  });
});

describe('rankModels', () => {
  it('orders by score then id, drops non-matches, and caps the list', () => {
    expect(rankModels('gpt-4', IDS)).toEqual(['gpt-4.1', 'gpt-4.1-mini']);
    expect(rankModels('mini', IDS)).toEqual(['gpt-4.1-mini', 'gpt-5-mini', 'o4-mini']);
    expect(rankModels('gpt-5', IDS)).toEqual(['gpt-5', 'gpt-5-mini']);
    expect(rankModels('zzz', IDS)).toEqual([]);
    expect(rankModels('', IDS, 3)).toEqual(['gpt-4.1', 'gpt-4.1-mini', 'gpt-5']);
  });
  it('lets subsequence matches through when nothing closer exists', () => {
    expect(rankModels('tes3', IDS)).toEqual(['text-embedding-3-small']);
  });
});
