# Run in Windows PowerShell -STA with a built TextLens instance running.
# This fixture does not synthesize input or inspect the TextLens process.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()

$form = New-Object System.Windows.Forms.Form
$form.Text = 'TextLens Windows gesture fixture'
$form.ClientSize = New-Object System.Drawing.Size(880, 630)
$form.StartPosition = 'CenterScreen'
$form.AutoScaleMode = 'Dpi'
$font = New-Object System.Drawing.Font('Microsoft YaHei UI', 12)
$form.Font = $font

$instructions = New-Object System.Windows.Forms.Label
$instructions.Location = New-Object System.Drawing.Point(12, 10)
$instructions.Size = New-Object System.Drawing.Size(850, 56)
$instructions.Text = '双击可选文字（至少两个字符）应显示工具栏。双击空白、按钮、列表项不应显示。请分别验证拖选、同词重选及关闭结果后立即重选。'
$form.Controls.Add($instructions)

$richLabel = New-Object System.Windows.Forms.Label
$richLabel.Location = New-Object System.Drawing.Point(12, 75)
$richLabel.Size = New-Object System.Drawing.Size(420, 28)
$richLabel.Text = 'RichTextBox：双击中文或英文单词'
$form.Controls.Add($richLabel)

$rich = New-Object System.Windows.Forms.RichTextBox
$rich.Location = New-Object System.Drawing.Point(12, 108)
$rich.Size = New-Object System.Drawing.Size(420, 160)
$rich.Text = '中文测试 双击选中 两个文字' + [Environment]::NewLine + 'TextLens selection testing' + [Environment]::NewLine + '空白后面                        '
$form.Controls.Add($rich)

$textLabel = New-Object System.Windows.Forms.Label
$textLabel.Location = New-Object System.Drawing.Point(12, 284)
$textLabel.Size = New-Object System.Drawing.Size(420, 28)
$textLabel.Text = 'Edit：双击词语、短拖选、Shift 选中'
$form.Controls.Add($textLabel)

$edit = New-Object System.Windows.Forms.TextBox
$edit.Location = New-Object System.Drawing.Point(12, 317)
$edit.Size = New-Object System.Drawing.Size(420, 40)
$edit.Text = '第二段文字 AnotherSelectedWord'
$form.Controls.Add($edit)

$passwordLabel = New-Object System.Windows.Forms.Label
$passwordLabel.Location = New-Object System.Drawing.Point(12, 375)
$passwordLabel.Size = New-Object System.Drawing.Size(420, 28)
$passwordLabel.Text = '密码框：任何选择均不应显示工具栏'
$form.Controls.Add($passwordLabel)

$password = New-Object System.Windows.Forms.TextBox
$password.Location = New-Object System.Drawing.Point(12, 408)
$password.Size = New-Object System.Drawing.Size(420, 40)
$password.UseSystemPasswordChar = $true
$password.Text = 'fixture-password'
$form.Controls.Add($password)

$listLabel = New-Object System.Windows.Forms.Label
$listLabel.Location = New-Object System.Drawing.Point(470, 75)
$listLabel.Size = New-Object System.Drawing.Size(390, 28)
$listLabel.Text = '对象列表：双击项目不能弹出'
$form.Controls.Add($listLabel)

$list = New-Object System.Windows.Forms.ListBox
$list.Location = New-Object System.Drawing.Point(470, 108)
$list.Size = New-Object System.Drawing.Size(390, 110)
[void]$list.Items.Add('测试文件夹')
[void]$list.Items.Add('ExampleApp.exe')
[void]$list.Items.Add('Report document')
$form.Controls.Add($list)

$button = New-Object System.Windows.Forms.Button
$button.Location = New-Object System.Drawing.Point(470, 234)
$button.Size = New-Object System.Drawing.Size(390, 45)
$button.Text = '双击按钮不能显示 TextLens 工具栏'
$form.Controls.Add($button)

$blank = New-Object System.Windows.Forms.Panel
$blank.Location = New-Object System.Drawing.Point(470, 298)
$blank.Size = New-Object System.Drawing.Size(390, 150)
$blank.BackColor = [System.Drawing.Color]::WhiteSmoke
$blank.BorderStyle = 'FixedSingle'
$form.Controls.Add($blank)

$status = New-Object System.Windows.Forms.Label
$status.Location = New-Object System.Drawing.Point(12, 470)
$status.Size = New-Object System.Drawing.Size(850, 125)
$status.Text = '观察：文字选中后应显示工具栏；按钮、列表、空白区域不可触发。'
$form.Controls.Add($status)

$rich.Add_SelectionChanged({
    $status.Text = "RichTextBox 当前选区长度：$($rich.SelectionLength)；请观察工具栏。"
})
$edit.Add_MouseUp({
    $status.Text = "Edit 当前选区长度：$($edit.SelectionLength)；请观察工具栏。"
})
$list.Add_DoubleClick({ $status.Text = '已双击列表项：TextLens 工具栏不应出现。' })
$button.Add_Click({ $status.Text = '按钮收到点击：TextLens 工具栏不应出现。' })
$blank.Add_DoubleClick({ $status.Text = '已双击空白区域：TextLens 工具栏不应出现。' })
[void]$form.ShowDialog()
$font.Dispose()
$form.Dispose()
