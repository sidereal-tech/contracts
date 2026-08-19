WAD=10**18; BPS=10_000; DAY=86400; YEAR=365*DAY
MAXP=(WAD*96)//100
LN2=693_147_180_559_945_309

def tdiv(a,b):  # rust i128 '/' truncates toward zero
    q=abs(a)//abs(b)
    return q if (a<0)==(b<0) else -q

def mdd(a,b,d): return tdiv(a*b,d)
def mdu(a,b,d):
    p=a*b; q=tdiv(p,d)
    return q if p%d==0 else q+1

def ln_wad(v):
    if v<=0: return None
    k=0; m=v
    while m>=2*WAD: m//=2; k+=1
    while m<WAD: m*=2; k-=1
    z=tdiv((m-WAD)*WAD,(m+WAD)); z2=tdiv(z*z,WAD)
    term=z; s=z; n=3
    while n<=49:
        term=tdiv(term*z2,WAD); s+= tdiv(term,n); n+=2
    return k*LN2+2*s

def exp_wad(v):
    k=tdiv(v+LN2//2,LN2) if v>=0 else tdiv(v-LN2//2,LN2)
    r=v-k*LN2
    term=WAD; s=WAD; i=1
    while i<=20:
        term=tdiv(tdiv(term*r,WAD),i)
        if term==0: break
        s+=term; i+=1
    if k>=0:
        if k>90: return None
        return s*(1<<k)
    sh=-k
    if sh>=127: return 0
    return s>>sh

def logprop(p):
    c=WAD-p
    if c<=0: return None
    return ln_wad(mdd(p,WAD,c))

def rate_scalar(root,t): return (root*YEAR)//t

def rate_anchor(P,lastln,A,rs,t):
    ex=exp_wad(mdd(lastln,t,YEAR))
    assert ex>=WAD,("anchor ex<WAD",ex)
    p=mdd(P,WAD,P+A); lp=logprop(p)
    a=ex-mdd(lp,WAD,rs)
    assert a>=0,"anchor negative"
    return a

def xrate(P,A,rs,anch,net):
    num=P-net; den=P+A
    if num<=0 or den<=0: return None
    p=mdd(num,WAD,den)
    if p<=0 or p>MAXP: return None
    lp=logprop(p)
    if lp is None: return None
    e=mdd(lp,WAD,rs)+anch
    return e if e>=WAD else None

def ln_implied(P,A,rs,anch,t):
    e=xrate(P,A,rs,anch,0); return mdd(ln_wad(e),YEAR,t)

class Pool:
    def __init__(s,P,A,root,anchor0,fee,mat,now):
        s.P=P; s.A=A; s.root=root; s.fee=fee; s.mat=mat; s.now=now
        t=mat-now; rs=rate_scalar(root,t)
        s.lastln=ln_implied(P,A,rs,anchor0,t)
    def comp(s):
        t=s.mat-s.now; rs=rate_scalar(s.root,t)
        return t,rs,rate_anchor(s.P,s.lastln,s.A,rs,t)
    def sell_pt(s,x):   # pt in -> sy out
        t,rs,anch=s.comp()
        e=xrate(s.P,s.A,rs,anch,-x)
        if e is None: return None
        pre=mdd(x,WAD,e); fee=mdd(pre,s.fee,BPS); out=pre-fee
        if out<=0 or out>=s.A: return None
        return out,e
    def buy_pt_cost(s,x):  # pt out -> sy in required
        t,rs,anch=s.comp()
        if x<=0 or x>=s.P: return None
        e=xrate(s.P,s.A,rs,anch,x)
        if e is None: return None
        pre=mdu(x,WAD,e); fee=mdu(pre,s.fee,BPS)
        return pre+fee,e
    def apply_sell(s,x):
        r=s.sell_pt(x); assert r,"sell fail"
        out,e=r; t,rs,anch=s.comp()
        s.P+=x; s.A-=out
        s.lastln=ln_implied(s.P,s.A,rs,anch,t)
        return out
    def apply_buy(s,x,syin):
        r=s.buy_pt_cost(x); assert r,"buy fail"
        cost,e=r; assert cost<=syin
        t,rs,anch=s.comp()
        s.P-=x; s.A+=syin
        s.lastln=ln_implied(s.P,s.A,rs,anch,t)
        return cost
