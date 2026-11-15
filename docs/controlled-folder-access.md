# "허용되지 않은 변경이 차단됨" 알림 해결

Windows 보안의 "제어된 폴더 액세스"(랜섬웨어 방지)가 켜져 있으면, oddity-picview 로 파일을 고칠 때 아래 알림이 뜨고 작업이 실패할 수 있다.

> 허용되지 않은 변경이 차단됨 — 제어된 폴더 액세스가 oddity-picview.exe을(를) 변경하지 못하도록 차단했습니다.

## 왜 뜨나

- 제어된 폴더 액세스는 문서, 사진, 동영상, 음악, 바탕 화면, 즐겨찾기 폴더(OneDrive 안의 같은 폴더 포함)에 허락받지 않은 프로그램이 쓰는 것을 막는다.
- oddity-picview 는 서명이 없는 새 프로그램이라 Windows 가 믿는 프로그램 목록에 없다.
- 아래 작업은 모두 그 폴더에 "쓰기"이므로 막힌다.
  - 회전 저장 (R / L, 툴바의 회전)
  - 이름 바꾸기 (F2), 새 폴더 (Ctrl+N)
  - 휴지통으로 보내기 (Delete)
  - 붙여넣기, 끌어 놓기로 복사하거나 옮기기 (보내는 곳이나 원본이 보호 폴더일 때)
- 막히지 않는 것: 그림 보기, 썸네일 만들기(썸네일은 `%LOCALAPPDATA%\oddity-picview\thumbs` 에 저장한다), 설정 저장(`%APPDATA%`).

알림의 "해제" 단추는 알림 창만 닫는다. 같은 작업을 하면 또 막힌다.

## 해결 방법

| 방법 | 랜섬웨어 방지 | 손이 가는 정도 | 남는 위험 |
|---|---|---|---|
| 1. oddity-picview.exe 를 허용 목록에 넣기 (권장) | 그대로 켜짐 | 한 번, 관리자 권한 | 허용한 프로그램은 보호 폴더 전체에 쓸 수 있다 |
| 2. 정리할 사진을 보호 폴더 밖에 두기 | 그대로 켜짐 | 사진을 옮겨야 함 | 없음 |
| 3. 감사 모드로 바꾸기 | 막지 않고 기록만 | 명령 한 줄 | 랜섬웨어를 막지 못한다 |
| 4. 제어된 폴더 액세스 끄기 | 꺼짐 | 스위치 하나 | 랜섬웨어를 막지 못한다 |

### 1. 허용 목록에 넣기 (권장)

1. 시작 → "Windows 보안" → 바이러스 및 위협 방지 → 랜섬웨어 방지 관리
2. "제어된 폴더 액세스를 통해 앱 허용" → "허용되는 앱 추가" → "모든 앱 찾아보기"
3. 쓰고 있는 `oddity-picview.exe` 를 고른다 (받은 곳, 또는 직접 빌드했으면 `target\release\oddity-picview.exe`).

관리자 PowerShell 에서는 한 줄이다 (경로는 실제 exe 위치로 바꾼다).

```
Add-MpPreference -ControlledFolderAccessAllowedApplications "C:\Tools\oddity-picview.exe"
```

- 장점: 다른 프로그램은 여전히 막힌다. 허용은 경로 기준이라, 같은 자리에 새 버전을 덮어써도 다시 할 필요가 없다.
- 단점: exe 를 다른 폴더로 옮기면 다시 넣어야 한다. 허용한 exe 를 누가 바꿔치기하면 그 프로그램도 보호 폴더에 쓸 수 있으니, exe 는 관리자만 쓸 수 있는 폴더(예: `C:\Program Files\oddity-picview`)에 두는 편이 낫다.

### 2. 보호 폴더 밖에서 정리하기

정리할 사진을 `D:\사진정리` 처럼 보호받지 않는 폴더에 두고 oddity-picview 로 정리한다.

- 장점: 허용 목록도, 보호 끄기도 필요 없다.
- 단점: 사진이 `사진` 폴더나 OneDrive 에 있으면 옮겨야 한다. 정리한 결과를 다시 보호 폴더로 넣을 때는 탐색기로 옮긴다.

### 3. 감사 모드

관리자 PowerShell:

```
Set-MpPreference -EnableControlledFolderAccess AuditMode
```

- 장점: 아무것도 막지 않으면서, 무엇이 막혔을지는 이벤트 기록(ID 1124)에 남는다. 어떤 프로그램을 허용할지 고를 때 쓰기 좋다.
- 단점: 그동안은 랜섬웨어도 막지 않는다. 확인이 끝나면 `Enabled` 로 되돌린다.

### 4. 끄기

Windows 보안 → 바이러스 및 위협 방지 → 랜섬웨어 방지 관리 → "제어된 폴더 액세스" 를 끔.

- 장점: 가장 간단하다. 다른 프로그램의 차단도 함께 사라진다.
- 단점: 보호 폴더가 랜섬웨어에 그대로 노출된다. OneDrive 의 "파일 복원"(최근 30일)으로 되살릴 수는 있지만, 동기화가 꺼진 파일은 되살릴 수 없다.

## 지금 상태 확인

켜짐 여부 (`0` 꺼짐, `1` 켜짐, `2` 감사 모드), 일반 PowerShell:

```
(Get-MpPreference).EnableControlledFolderAccess
```

oddity-picview 가 막힌 기록:

```
Get-WinEvent -FilterHashtable @{LogName='Microsoft-Windows-Windows Defender/Operational'; Id=1123} |
  Where-Object { $_.Message -match 'oddity-picview' } | Select-Object -First 5 TimeCreated, Message
```
