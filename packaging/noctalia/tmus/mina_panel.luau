--!nonstrict
local M = {width=1120,height=760,active=false,menu=nil,confirm=nil,eqDraft={},eqGrab=nil,eqHover=nil}
local C = {bg='#151217',surface='#20171E',text='#E9DCCB',muted='#BCAEAE',accent='#D7AE91',rule='#59414A',red='#D76C82',selected='#442330',playing='#542838'}
local clear = '#00000000'
local FONT, TITLE = 'Maple Mono', 'Noto Serif'
local names = {{'home','Главная'},{'search','Поиск'},{'library','Библиотека'},{'clients','Источники'},{'eq','Эквалайзер'}}

function M.enabled()
  return noctalia.getSetting('bar.mina-left.enabled') == true
end
function M.label(key,text,props)
  props = props and table.clone(props) or {}
  props.key,props.text = key,tostring(text or '')
  props.fontFamily,props.fontSize = props.fontFamily or FONT,props.fontSize or 13
  props.color,props.maxLines = props.color or C.text,props.maxLines or 1
  return ui.label(props)
end
function M.rule(key,hot)
  return ui.separator({key=key,color=hot and C.accent or C.rule,thickness=1})
end
function M.button(key,text,fn,props)
  props = props or {}
  return ui.button({key=key,text=text,glyph=props.glyph,onClick=fn,height=props.height or 30,width=props.width,
    flexGrow=props.flexGrow,enabled=props.enabled~=false,variant='ghost',radius=props.radius or 4,
    fontSize=props.fontSize or 12,color=props.active and C.text or C.muted,
    fill=props.active and C.selected or clear,hoverFill='#432530',pressedFill='#623342',border=props.active and C.rule or clear,
    borderWidth=props.active and 1 or 0,hintIdentity=not props.noHints and (props.identity or key) or nil,tooltip=props.tooltip or text})
end
function M.icon(key,glyph,tooltip,fn,props)
  props=props and table.clone(props) or {}
  props.glyph,props.tooltip,props.width=glyph,tooltip,props.width or 30
  return M.button(key,'',fn,props)
end
function M.art(key,path,size)
  if type(path)=='string' and path~='' then
    return ui.image({key=key,path=path,width=size,height=size,fit='cover',radius=3,border=C.rule,borderWidth=1})
  end
  return ui.box({key=key,width=size,height=size,fill=C.surface,border=C.rule,borderWidth=1,radius=3},{
    ui.glyph({key=key..'-empty',name='music',x=(size-20)/2,y=(size-20)/2,size=20,color=C.muted}),
  })
end
function M.topInset()
  -- The Mina sidebar is 671px tall; its portrait starts at 98px. Both surfaces share their center.
  return math.max(12,math.floor((M.height-671)/2+76))
end
function M.layout(view)
  local width=math.max(240,M.width-46)
  local narrow=M.width<880
  local height=math.max(140,M.height-(narrow and 240 or 224)-M.topInset()+12)
  local queue=narrow and width or math.clamp(math.floor(width*.265),220,280)
  local main=narrow and width or width-queue-28
  local controls=view=='search' and 112 or view=='library' and 240 or view=='playlist' and 86 or 48
  return {width=width,height=height,main=main,queue=queue,narrow=narrow,
    tracks=math.max(1,math.floor((height-controls)/46)),queueRows=math.max(1,math.floor((height-80)/54)),
    homeRows=math.max(1,math.floor((height-48)/186)),columns=math.clamp(math.floor(main/110),2,6)}
end
function M.track(model,row,index)
  local key='mina-track-'..index
  local chosen=index==model.cursor and model.focus==2
  local playing=row.id==tostring(model.snapshot.trackId or '')
  local identity=model.view..':'..model.playlist.id..':'..index..':'..tostring(row.id or row.open or '')
  local metadata=row.subtitle or ''
  local provider=model.provider(row)
  if metadata=='' then metadata=provider end
  if row.kind=='artist' then metadata='Исполнитель'
  elseif row.kind=='playlist' then metadata='Плейлист' end
  local nodes={
    M.label(key..'-index',playing and '▸' or tostring(index),{width=20,color=playing and C.red or C.muted,fontSize=11}),
    M.art(key..'-art',row.art,32),
    ui.column({key=key..'-text',flexGrow=1,gap=2},{
      M.label(key..'-title',row.title,{fontSize=13}),
      M.label(key..'-sub',metadata,{fontSize=11,color=C.muted}),
    }),
  }
  if row.kind=='track' then
    nodes[#nodes+1]=M.icon(key..'-like','heart',row.rating=='liked' and 'Убрать лайк' or 'Нравится',function() model.trackAction('like',row,index) end,
      {active=row.rating=='liked',width=28,identity=identity..':like'})
  end
  nodes[#nodes+1]=M.label(key..'-duration',row.time or '',{width=42,color=C.muted,fontSize=11,textAlign='end'})
  nodes[#nodes+1]=M.button('mina-track-menu-'..index,'···',function() model.trackAction('menu',row,index) end,
    {width=28,identity=identity..':menu',tooltip='Действия: '..tostring(row.title)})
  return ui.column({key=key..'-wrap',gap=0,height=46},{
    ui.row({key=key,align='center',gap=8,paddingH=7,height=45,radius=3,
      fill=playing and C.playing or chosen and C.selected or clear,border=chosen and C.accent or clear,borderWidth=chosen and 1 or 0,
      hintIdentity=identity..':select',onClick=function() model.select(index) end},nodes),
    M.rule(key..'-rule',false),
  })
end
function M.trackList(model)
  local nodes={}
  for n,row in ipairs(model.items) do
    local i=model.first+n-1
    nodes[#nodes+1]=row.kind=='more' and M.label('mina-load-more',row.title,{color=C.muted}) or M.track(model,row,i)
  end
  return ui.column({key='mina-track-list',gap=0},nodes)
end
function M.collection(model,row,index,width)
  local key='mina-playlist-'..index
  local selected=index==model.cursor
  local art=math.min(94,width-8)
  return ui.column({key=key,width=width,height=150,gap=5,padding=4,fill=clear,
    hintIdentity='library:'..index..':'..tostring(row.open),onClick=function() model.select(index) end},{
    ui.row({height=art},{M.art(key..'-art',row.art,art)}),
    M.label(key..'-title',row.title,{fontSize=12}),
    M.label(key..'-sub',row.subtitle,{fontSize=10,color=C.muted}),
    M.rule(key..'-rule',selected and model.focus==2),
  })
end
function M.library(model)
  local cards={}
  local width=math.floor((model.layout.main-10*(model.columns-1))/model.columns)
  for n,row in ipairs(model.items) do
    cards[#cards+1]=M.collection(model,row,model.first+n-1,width)
  end
  local out={ui.row({key='mina-library-collections',gap=10,height=150},cards),
    ui.row({key='mina-library-track-heading',height=32,gap=8,align='center'},{
      M.label('mina-library-track-title',model.previewTitle or 'Треки в коллекции',{fontFamily=TITLE,fontSize=17,flexGrow=1}),
      M.button('mina-library-open','Открыть плейлист',model.openCollection,{width=154,enabled=model.previewId~=nil}),
    })}
  if model.previewMessage then
    out[#out+1]=M.label('mina-library-tracks-state',model.previewMessage,{color=C.muted,maxLines=3})
  else
    for n,row in ipairs(model.preview or {}) do
      local i=n
      out[#out+1]=ui.row({key='mina-preview-'..i,height=44,gap=8,paddingH=7,align='center',
        onClick=function() model.command('play-playlist',model.previewId..'|'..row.id) end,
        hintIdentity='preview:'..model.previewId..':'..i..':'..row.id},{
        M.art('mina-preview-art-'..i,row.art,30),
        ui.column({flexGrow=1,gap=2},{M.label('mina-preview-title-'..i,row.title),M.label('mina-preview-artist-'..i,row.subtitle,{fontSize=10,color=C.muted})}),
        M.icon('mina-preview-like-'..i,'heart',row.rating=='liked' and 'Убрать лайк' or 'Нравится',function() model.trackAction('like',row,i) end,{width=28,active=row.rating=='liked'}),
        M.label('mina-preview-duration-'..i,row.time,{width=44,fontSize=11,color=C.muted,textAlign='end'}),
        M.button('mina-preview-menu-'..i,'···',function() model.trackAction('menu',row,i) end,{width=28,tooltip='Действия: '..row.title}),
      })
    end
  end
  return ui.column({key='mina-library',gap=8},out)
end
function M.home(model)
  local shelves={}
  for _,shelf in ipairs(model.home) do
    local cards={}
    local width=math.floor((model.layout.main-24)/3)
    for _,entry in ipairs(shelf.cards) do
      local key='mina-home-'..shelf.index..'-'..entry.index
      cards[#cards+1]=ui.column({key=key,width=width,gap=4,padding=3,hintIdentity=key..':'..tostring(entry.row.id),onClick=entry.select},{
        ui.row({height=92},{M.art(key..'-art',entry.row.art,92)}),
        M.label(key..'-title',entry.row.title,{fontSize=12}),
        M.label(key..'-artist',entry.row.subtitle,{fontSize=10,color=C.muted}),
        M.button(key..'-open',entry.row.kind=='track' and 'Слушать' or 'Открыть',entry.activate,{height=26}),
        M.rule(key..'-rule',entry.chosen and model.focus==2),
      })
    end
    shelves[#shelves+1]=ui.column({key='mina-shelf-'..shelf.index,height=186,gap=5},{
      M.label('mina-shelf-title-'..shelf.index,shelf.title,{fontFamily=TITLE,fontSize=17}),
      ui.row({key='mina-shelf-cards-'..shelf.index,gap=12},cards),
    })
  end
  if #shelves==0 then shelves={M.label('mina-home-state',model.homeMessage,{color=C.muted,maxLines=3})} end
  return ui.column({key='mina-home',gap=10},shelves)
end
function M.clients(model)
  local out={}
  local auth={Ready='Подключено',Anonymous='Без входа',Missing='Нет сессии',Expired='Сессия истекла'}
  for n,row in ipairs(model.items) do
    local i=model.first+n-1
    if not row.restart then
      local meta=model.auth(row.payload)
      out[#out+1]=ui.column({key='mina-client-wrap-'..i,gap=0,height=70},{
        ui.row({key='mina-client-'..i,height=69,gap=12,paddingH=8,align='center',
          fill=i==model.cursor and model.focus==2 and C.selected or clear,
          hintIdentity='source:'..tostring(row.payload),onClick=function() model.activate(row,i) end},{
          M.label('mina-client-mark-'..i,row.active and '●' or '○',{width=20,color=row.active and C.red or C.muted}),
          ui.column({flexGrow=1,gap=4},{M.label('mina-client-name-'..i,row.payload=='all' and 'Все источники' or row.title,{fontSize=15}),
            M.label('mina-client-hint-'..i,meta and meta.authHint or row.subtitle,{fontSize=11,color=C.muted})}),
          M.label('mina-client-state-'..i,meta and (auth[meta.authKind] or 'Неизвестно') or '',{width=138,fontSize=11,color=C.muted}),
        }),M.rule('mina-client-rule-'..i,false),
      })
    end
  end
  return ui.column({key='mina-clients',gap=0},out)
end
function M.eq(model)
  local eq=model.snapshot.equalizer
  if type(eq)~='table' then return M.label('mina-eq-unavailable','Эквалайзер недоступен',{color=C.muted}) end
  local count=10
  local chartH=math.clamp(model.layout.height-208,160,300)
  local plotW=math.max(180,model.layout.main-52)
  local gains={}
  for i=1,count do
    local actual=tonumber((eq.bands or {})[i]) or 0
    if M.eqDraft[i]==actual and M.eqGrab~=i then M.eqDraft[i]=nil end
    gains[i]=M.eqDraft[i] or actual
  end
  local values=model.eqCurve(gains,count)
  table.insert(values,1,values[1])
  table.insert(values,1,values[1])
  local function y(gain) return (.94-.88*(gain+15)/30)*chartH end
  local grid,scale,handles,labels={},{},{},{}
  for gain=-15,15,3 do
    local top=y(gain)
    grid[#grid+1]=ui.box({key='mina-eq-grid-'..gain,x=0,y=top,width=plotW,height=1,fill=gain==0 and '#79616A' or '#352A32'})
    scale[#scale+1]=M.label('mina-eq-db-'..gain,(gain>0 and '+' or '')..gain,{x=0,y=top-7,width=42,fontSize=10,color=C.muted,textAlign='end'})
  end
  for i=1,count do
    local gain=gains[i]
    grid[#grid+1]=ui.box({key='mina-eq-frequency-grid-'..i,x=(i-.5)/count*plotW,y=0,width=1,height=chartH,fill='#292129'})
    local chosen=model.cursor==i+2 and model.focus==2
    handles[#handles+1]=ui.box({key='mina-eq-point-'..i,x=(i-.5)/count*plotW-4,y=y(gain)-4,width=8,height=8,
      radius=4,fill=M.eqGrab==i and C.text or C.accent,border=C.red,borderWidth=1})
    labels[#labels+1]=M.button('mina-eq-band-'..i,({'32','64','125','250','500','1k','2k','4k','8k','16k'})[i],
      function() model.eqFocus(i) end,{width=plotW/count,height=25,fontSize=10,active=chosen,tooltip=string.format('%s · %+g dB',model.frequencies[i],gain)})
  end
  local plot={}
  for _,node in ipairs(grid) do plot[#plot+1]=node end
  plot[#plot+1]=ui.graph({key='mina-eq-curve',width=plotW,height=chartH,values=values,color='#C44A62',lineWidth=1.2,fillOpacity=.45,
    onPointerMove='minaEqMove',onPointerLeave='minaEqLeave'})
  for _,node in ipairs(handles) do plot[#plot+1]=node end
  local selected=M.eqGrab or math.clamp(model.cursor-2,1,count)
  return ui.column({key='mina-eq',gap=12},{
    ui.row({key='mina-eq-controls',gap=10,height=32,align='center'},{
      M.button('mina-eq-power',eq.enabled and '●  Включён' or '○  Выключен',function() model.command('eq',eq.enabled and 'off' or 'on') end,{width=128,active=eq.enabled}),
      M.button('mina-eq-preset','Preset: '..tostring(eq.preset or 'Flat')..' ▾',model.presets,{flexGrow=1}),
      M.button('mina-eq-flat','Flat',function() M.resetEq(); model.command('eq','flat') end,{width=60}),
    }),
    ui.row({key='mina-eq-chart',gap=10,height=chartH},{ui.box({key='mina-eq-scale',width=42,height=chartH,fill=clear},scale),
      ui.box({key='mina-eq-hit',width=plotW,height=chartH,fill=clear,onClick=model.eqClick,hintIdentity='eq-graph'},plot)}),
    ui.row({key='mina-eq-labels',gap=0,height=25},{ui.box({width=52,fill=clear}),ui.row({gap=0,width=plotW},labels)}),
    M.label('mina-eq-value',string.format('%s · %+g dB',model.frequencies[selected],gains[selected]),{fontSize=12,color=C.accent}),
    M.label('mina-eq-help',M.eqGrab and 'Измените высоту указателя; клик или выход из графика применяет значение.' or 'Клик по кривой выбирает полосу; второй клик применяет. h/l: ±1 dB.',{fontSize=11,color=C.muted,maxLines=2}),
  })
end
function M.resetEq()
  M.eqGrab,M.eqHover,M.eqDraft=nil,nil,{}
end
function M.transport(model)
  local s=model.snapshot
  local present=s.linked==true and tostring(s.trackId or '')~=''
  local controls={
    M.icon('mina-now-like','heart',model.nowLiked and 'Убрать лайк' or 'Нравится',model.nowLike,{active=model.nowLiked,enabled=present}),
    M.icon('mina-shuffle','arrows-shuffle','Перемешивание',function() model.command('shuffle','toggle') end,{active=s.shuffle,enabled=s.linked}),
    M.icon('mina-prev','player-skip-back','Предыдущий трек',function() model.command('prev') end,{enabled=s.linked}),
    M.icon('mina-toggle',s.status=='playing' and 'player-pause' or 'player-play',s.status=='playing' and 'Пауза' or 'Слушать',function() model.command('toggle') end,{width=38,height=38,active=true,enabled=s.linked}),
    M.icon('mina-next','player-skip-forward','Следующий трек',function() model.command('next') end,{enabled=s.linked}),
    M.button('mina-repeat',s.loop=='track' and '↻¹' or '↻',model.repeatNext,{width=30,active=s.loop~='none' and s.loop~=nil,enabled=s.linked,tooltip='Повтор: '..tostring(s.loop or 'none')}),
  }
  local volumeWidth=M.width<880 and 86 or 126
  local nodes={M.art('mina-now-art',s.artPath,44),
    ui.column({key='mina-now-meta',width=M.width<880 and 110 or 194,gap=3},{
      M.label('mina-now-title',present and s.title or s.linked and 'Нет текущего трека' or 'tmusd недоступен',{fontSize=12}),
      M.label('mina-now-artist',present and s.artist or 'Выберите музыку в библиотеке',{fontSize=10,color=C.muted}),
    }),ui.row({key='mina-transport-controls',gap=3,align='center'},controls)}
  nodes[#nodes+1]=ui.slider({key='mina-seek',min=0,max=math.max(1,model.duration),step=1,value=model.position,
    flexGrow=1,enabled=present and model.duration>0,fill=C.red,track=C.rule,thumb=C.accent,focusColor=C.accent,
    onChange=model.seekDraft,onDragEnd=model.seekCommit})
  nodes[#nodes+1]=M.label('mina-time',model.time,{width=94,fontSize=10,color=C.muted,textAlign='end'})
  nodes[#nodes+1]=M.icon('mina-volume-mode','volume','Громкость музыки: '..model.volume..'%',model.volumeFocus,{width=24,active=model.volumeKeys})
  nodes[#nodes+1]=ui.slider({key='mina-volume',width=volumeWidth,min=0,max=100,step=1,value=model.volume,enabled=s.linked,
    fill=C.red,track=C.rule,thumb=C.accent,focusColor=C.accent,onChange=model.volumeDraft,onDragEnd=model.volumeCommit})
  nodes[#nodes+1]=M.icon('mina-now-actions','dots','Действия с текущим треком',model.nowActions,{enabled=present})
  if M.width<880 then
    local upper,lower={},{}
    for i,node in ipairs(nodes) do table.insert(i<=3 and upper or lower,node) end
    return ui.box({key='mina-transport',x=20,y=M.height-112,width=M.width-44,height=70,fill=clear},{
      ui.row({key='mina-transport-upper',width=M.width-44,height=40,gap=8,align='center'},upper),
      ui.row({key='mina-transport-lower',y=46,width=M.width-44,height=18,gap=8,align='center'},lower),
    })
  end
  return ui.row({key='mina-transport',x=20,y=M.height-96,width=M.width-44,height=60,gap=8,align='center'},nodes)
end
function M.queue(model)
  local nodes={ui.row({key='mina-queue-heading',width=model.layout.queue,height=36,gap=8,align='center'},{
    M.label('mina-queue-title','Очередь',{fontFamily=TITLE,fontSize=19,flexGrow=1,color=model.focus==3 and C.accent or C.text}),
    M.label('mina-queue-count',tostring(#(model.snapshot.queue or {})),{fontSize=11,color=C.muted}),
    M.icon('mina-queue-modes','adjustments-horizontal','Режимы очереди',model.modes,{width=28}),
  })}
  if #(model.snapshot.queue or {})==0 then nodes[#nodes+1]=M.label('mina-queue-empty','Очередь пуста',{fontSize=12,color=C.muted}) end
  for _,entry in ipairs(model.queue) do
    local i=entry.index
    local key='mina-queue-'..i
    nodes[#nodes+1]=ui.row({key=key,width=model.layout.queue,height=52,paddingH=6,gap=7,align='center',radius=3,
      fill=entry.playing and C.playing or entry.chosen and model.focus==3 and C.selected or clear,
      border=entry.chosen and model.focus==3 and C.accent or clear,borderWidth=entry.chosen and model.focus==3 and 1 or 0,
      hintIdentity='queue:'..i..':'..entry.id,onClick=entry.select},{
      M.label(key..'-index',entry.playing and '▸' or tostring(i),{width=14,fontSize=10,color=entry.playing and C.red or C.muted}),
      M.art(key..'-art',entry.row.art_path,30),
      ui.column({key=key..'-text',flexGrow=1,gap=3},{M.label(key..'-title',entry.row.title,{fontSize=11}),M.label(key..'-artist',entry.artist,{fontSize=10,color=C.muted})}),
      M.icon('mina-queue-play-'..i,'player-play','Слушать: '..tostring(entry.row.title),entry.play,{width=24,enabled=model.snapshot.linked,identity='queue-play:'..i..':'..entry.id}),
    })
  end
  if #(model.snapshot.queue or {})>model.layout.queueRows then
    nodes[#nodes+1]=ui.row({key='mina-queue-pages',width=model.layout.queue,height=28,gap=8,align='center'},{
      M.button('mina-queue-earlier','‹',function() model.queuePage(-1) end,{width=30,enabled=model.queueFirst>1,tooltip='Предыдущая часть очереди'}),
      M.label('mina-queue-range',model.queueFirst..'–'..(model.queueFirst+#model.queue-1),{flexGrow=1,fontSize=10,color=C.muted,textAlign='center'}),
      M.button('mina-queue-later','›',function() model.queuePage(1) end,{width=30,enabled=model.queueFirst+#model.queue-1<#model.snapshot.queue,tooltip='Следующая часть очереди'}),
    })
  end
  return ui.column({key='mina-queue-content',width=model.layout.queue,gap=2},nodes)
end
function M.overlay(model)
  local x=math.max(26,M.width-326)
  local width=math.min(320,M.width-52)
  local props={x=x,y=120+M.topInset(),width=width,padding=16,gap=8,fill=C.surface,border=C.accent,borderWidth=1,radius=5}
  if model.naming then
    props.key='mina-naming'
    return ui.column(props,{
      M.label('mina-naming-title','Новый плейлист',{fontFamily=TITLE,fontSize=20}),
      M.label('mina-naming-text',model.name..'▌',{fontSize=16}),
      M.label('mina-naming-help','Enter: создать · Esc: отмена',{fontSize=11,color=C.muted}),
    })
  end
  if M.info then
    props.key='mina-info'
    local entries={M.label('mina-info-title',M.info.title,{fontFamily=TITLE,fontSize=18,maxLines=2})}
    for i,line in ipairs(M.info.lines) do
      entries[#entries+1]=ui.column({key='mina-info-line-'..i,gap=3},{
        M.label('mina-info-key-'..i,line[1],{fontSize=10,color=C.muted}),
        M.label('mina-info-value-'..i,line[2],{fontSize=12,maxLines=2}),
      })
    end
    entries[#entries+1]=M.button('mina-info-close','Закрыть',model.cancel,{height=30})
    return ui.column(props,entries)
  end
  if model.picker then
    props.key='mina-picker'
    local entries={M.label('mina-picker-title','В плейлист',{fontFamily=TITLE,fontSize=18})}
    local count=math.max(1,math.floor((model.layout.height-64)/36))
    local first=math.max(1,model.picker.cursor-count+1)
    for i=first,math.min(#model.picker.entries,first+count-1) do
      local item=model.picker.entries[i]
      entries[#entries+1]=M.button('mina-picker-'..i,item.title,item.fn,{height=30,active=i==model.picker.cursor})
    end
    return ui.column(props,entries)
  end
  if M.confirm then
    props.key='mina-confirm'
    return ui.column(props,{
      M.label('mina-confirm-title',M.confirm.title,{fontFamily=TITLE,fontSize=18,maxLines=4}),
      ui.row({gap=8,height=34},{
        M.button('mina-confirm-cancel','Отмена',model.cancel,{flexGrow=1,active=not M.confirm.apply}),
        M.button('mina-confirm-apply','Подтвердить',model.confirm,{flexGrow=1,active=M.confirm.apply}),
      }),
    })
  end
  if M.menu then
    props.key='mina-menu'
    local nodes={M.label('mina-menu-title',M.menu.title,{fontFamily=TITLE,fontSize=17,maxLines=2}),M.rule('mina-menu-rule',false)}
    local count=math.max(1,math.floor((model.layout.height-64)/36))
    M.menu.top=math.clamp(M.menu.top or 1,math.max(1,M.menu.cursor-count+1),M.menu.cursor)
    for i=M.menu.top,math.min(#M.menu.entries,M.menu.top+count-1) do
      local entry=M.menu.entries[i]
      nodes[#nodes+1]=M.button('mina-menu-'..entry.key,entry.title,entry.fn,{height=30,active=i==M.menu.cursor,enabled=entry.enabled~=false,identity=M.menu.identity..':'..entry.key})
    end
    return ui.column(props,nodes)
  end
  return nil
end
function M.render(model)
  local layout=model.layout
  local tabs={}
  for _,entry in ipairs(names) do
    local id,title=entry[1],entry[2]
    local cell=(layout.width-20)/5
    local pill=math.min(112,cell)
    local children={}
    if model.sectionId==id then children[#children+1]=ui.image({path='skin-assets/mina/tab-active.svg',x=(cell-pill)/2,y=2,width=pill,height=26,fit='stretch'}) end
    children[#children+1]=M.button('mina-tab-'..id,title,function() model.section(id) end,{height=30,width=cell,fontSize=12})
    tabs[#tabs+1]=ui.box({key='mina-tab-slot-'..id,width=cell,height=30,fill=clear},children)
  end
  local main={}
  if model.view=='search' then
    main[#main+1]=ui.row({key='mina-search-line',height=36,gap=8,paddingH=8,align='center',fill='#1E171D',border=C.rule,borderWidth=1,radius=4,onClick=model.searchFocus},{
      ui.glyph({name='search',size=15,color=C.muted}),model.searchText,M.button('mina-search-submit','Найти',model.searchSubmit,{width=74}),
    })
    local kinds,providers={},{}
    for _,item in ipairs(model.kinds) do kinds[#kinds+1]=M.button('mina-kind-'..item.id,item.title,item.fn,{height=26,flexGrow=1,active=item.active}) end
    for _,item in ipairs(model.providers) do providers[#providers+1]=M.button('mina-provider-'..item.id,item.title,item.fn,{height=26,flexGrow=1,active=item.active}) end
    main[#main+1]=ui.row({height=26,gap=6},kinds)
    main[#main+1]=ui.row({height=26,gap=6},providers)
  else
    local heading=model.view=='playlist' and model.playlist.title or ({home='Для вас',library='Моя коллекция',clients='Источники',eq='Эквалайзер'})[model.view]
    main[#main+1]=ui.row({key='mina-view-heading',height=34,gap=8,align='center'},{
      M.label('mina-view-title',heading,{fontFamily=TITLE,fontSize=21,flexGrow=1}),
      M.button(model.view=='clients' and 'mina-service-menu' or 'mina-view-action',model.viewActionTitle,model.viewAction,{width=142}),
    })
    if model.view=='playlist' then
      main[#main+1]=ui.row({height=28,gap=8},{M.button('mina-playlist-back','‹ Библиотека',model.back,{width=132}),M.button('mina-playlist-listen','Слушать',model.playPlaylist,{width=100}),M.label('mina-playlist-count',tostring(model.total)..' загружено',{fontSize=11,color=C.muted})})
    end
  end
  local body
  if model.help then body=model.help
  elseif model.message~='' then body=M.label('mina-state',model.message,{color=C.muted,maxLines=5,fontSize=14})
  elseif model.view=='home' then body=M.home(model)
  elseif model.view=='library' then body=M.library(model)
  elseif model.view=='clients' then body=M.clients(model)
  elseif model.view=='eq' then body=M.eq(model)
  else body=M.trackList(model) end
  if model.error~='' then main[#main+1]=M.label('mina-error',model.error,{fontSize=11,color='#E9A5B4',maxLines=2}) end
  main[#main+1]=ui.scroll({key='mina-view-scroll',flexGrow=1,padding=0,gap=0},{body})
  local top=M.topInset()
  local nodes={
    ui.box({key='mina-field',x=0,y=top,width=M.width-10,height=M.height-top-18,fill=C.bg,border='#9F7863',borderWidth=1,radius=12}),
    ui.box({key='mina-frame-inner',x=5,y=top+5,width=M.width-20,height=M.height-top-28,fill=clear,border=C.rule,borderWidth=1,radius=9}),
    ui.box({key='mina-header-shade',x=8,y=top+8,width=M.width-28,height=36,fill='#24171F'}),
    M.label('mina-panel-title','Noctalia // Audio',{x=28,y=top+12,fontFamily=TITLE,fontSize=19,color=C.accent}),
    ui.row({key='mina-window-actions',x=M.width-128,y=top+11,width=100,height=28,gap=8},{
      M.icon('mina-compact','picture-in-picture','Компактный плеер',model.compact,{width=28}),
      M.icon('mina-close','x','Закрыть',model.close,{width=28}),
    }),
    ui.row({key='mina-tabs',x=26,y=top+54,width=layout.width,height=30,gap=5},tabs),
    ui.box({key='mina-header-line',x=22,y=top+92,width=layout.width+6,height=1,fill=C.rule}),
    ui.box({key='mina-transport-line',x=15,y=M.height-(layout.narrow and 122 or 106),width=M.width-38,height=1,fill='#80505A'}),
  }
  for _,corner in ipairs({{'tl',0,top-2},{'tr',M.width-40,top-2},{'bl',0,M.height-60},{'br',M.width-40,M.height-60}}) do
    nodes[#nodes+1]=ui.image({key='mina-corner-'..corner[1],path='skin-assets/mina/corner-'..corner[1]..'.svg',x=corner[2],y=corner[3],width=32,height=42,fit='stretch'})
  end
  if not layout.narrow or model.focus~=3 then
    nodes[#nodes+1]=ui.column({key='mina-main-pane',x=26,y=top+106,width=layout.main,height=layout.height,gap=8},main)
  end
  if not layout.narrow then
    nodes[#nodes+1]=ui.box({key='mina-queue-divider',x=26+layout.main+13,y=top+114,width=1,height=layout.height-8,fill=C.rule})
  end
  if not layout.narrow or model.focus==3 then
    nodes[#nodes+1]=ui.column({key=layout.narrow and 'mina-queue-narrow' or 'mina-queue-wide',x=layout.narrow and 26 or 26+layout.main+28,y=top+106,width=layout.queue,height=layout.height,gap=0},{M.queue(model)})
  end
  nodes[#nodes+1]=M.transport(model)
  nodes[#nodes+1]=ui.row({key='mina-footer',x=26,y=M.height-44,width=layout.width,height=18,gap=12},{
    M.label('mina-keymap',model.volumeKeys and 'h/l: громкость · Esc: выход' or 'F: действия · ?: карта · Tab: каталог / очередь',{fontSize=10,color=C.muted,flexGrow=1}),
    M.button('mina-buffer-toggle',model.focus==3 and '‹ Каталог' or 'Очередь ›',model.toggleBuffer,{width=100,height=18,fontSize=10}),
  })
  local overlay=M.overlay(model)
  if overlay then
    local function inhibit(node)
      if node.props then
        node.props.onClick,node.props.onChange,node.props.onDragEnd=nil,nil,nil
        node.props.onPointerMove,node.props.onPointerLeave=nil,nil
        if node.props.enabled~=nil then node.props.enabled=false end
      end
      for _,child in ipairs(node.children or {}) do inhibit(child) end
    end
    for _,node in ipairs(nodes) do inhibit(node) end
    nodes[#nodes+1]=overlay
  end
  return ui.box({key='mina-music-panel',width=M.width,height=M.height,fill=clear,clip=true},nodes)
end
return M
