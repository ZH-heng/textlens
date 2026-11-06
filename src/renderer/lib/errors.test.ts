import { getErrorMessage } from './errors'

describe('native and renderer error messages', () => {
  it('keeps string rejections returned by Tauri Result<T, String> commands', () => {
    expect(getErrorMessage('  服务商已停用  ', '操作失败')).toBe('服务商已停用')
    expect(getErrorMessage(new Error('磁盘空间不足'))).toBe('磁盘空间不足')
  })
  it('falls back for empty or non-message payloads', () => {
    for (const error of ['', '   ', null, undefined, { token: 'do not expose arbitrary records' }]) {
      expect(getErrorMessage(error, '无法保存')).toBe('无法保存')
    }
  })
})
