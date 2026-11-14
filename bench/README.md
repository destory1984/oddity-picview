# 벤치마크

결과 표는 [README](../README.md#알씨와-속도-비교) 에 있다.

5번의 범위는 알씨 1.43 → 1.89초, oddity-picview 0.30 → 0.38초였다.

측정 조건:

- PC: AMD Ryzen 9 7900 (12코어 24스레드), 메모리 47GB, Windows 11 Pro
- 사진: 4032x3024(휴대폰 12MP 크기) JPEG 100장, 장당 1.6MB. [bench/make_photos.py](bench/make_photos.py) 로 만든 합성 사진이다.
- 창 크기는 1600x1000으로 맞췄다. 프로그램을 실행한 때부터 썸네일 칸이 마지막으로 바뀐 때까지를 쟀다. 2초 동안 바뀌지 않으면 끝난 것으로 봤다. 창은 0.05초마다 찍었으므로 ±0.05초 오차가 있다.
- "처음 여는 폴더"는 잴 때마다 사진 파일의 수정 시각을 바꿔서, 두 프로그램 모두 저장해 둔 썸네일을 쓰지 못하게 했다. 사진 파일 자체는 윈도우가 메모리에 올려 둔 상태라 디스크 읽는 시간은 거의 들어가지 않았다.
- 화면에 보인 썸네일 수가 다르다. 알씨는 5장(알씨 설정 그대로 250픽셀), oddity-picview는 9장(160픽셀)이다. oddity-picview가 더 많이 그리고도 빨랐다.
- 알씨는 켤 때 오른쪽 위 광고를 받아 온다. 광고 칸은 측정 범위에서 뺐지만, 광고를 받는 시간과 CPU는 알씨 수치에 들어 있다.
- 알씨는 다시 여는 폴더에서도 빨라지지 않았다. 썸네일을 디스크에 저장해 두지 않는 것으로 보인다.

다시 재는 방법 (bash):

```
python bench/make_photos.py bench/photos 100
python bench/time_to_thumbnails.py target/release/oddity-picview.exe bench/photos 0.21,0.17,0.74,0.95 bench/out/pv.png
```

[bench/time_to_thumbnails.py](bench/time_to_thumbnails.py) 는 클릭이나 키 입력 없이 창만 찍어서 재므로, 소스를 볼 수 없는 프로그램에도 쓸 수 있다. 세 번째 인자는 지켜볼 썸네일 칸의 위치(창 크기 대비 0 → 1)다. 알씨는 폴더를 인자로 넘기면 크게보기로 열리므로, 마지막으로 연 폴더를 다시 여는 `-` 로 실행했다.
