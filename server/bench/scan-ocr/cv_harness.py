import cv2,glob,os,math,json
meta={}
for p in sorted(glob.glob('/out/strips/*.png')):
    s=cv2.imread(p)  # BGR
    h,w=s.shape[:2]
    b=os.path.basename(p)[:-4]
    a=cv2.resize(s,(math.ceil(48*w/h),48)); cv2.imwrite(f'/out/cv/{b}_a.png',a)
    l=cv2.resize(s,(int(w*0.37)+1,35)); cv2.imwrite(f'/out/cv/{b}_l.png',l)
    u=cv2.resize(s,(w*2-3,h+17)); cv2.imwrite(f'/out/cv/{b}_u.png',u)
print('ok')
