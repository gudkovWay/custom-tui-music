--!nonstrict
local visual=require('./mina_panel.luau')
local snapshot={}
local opened=false
local W,H=360,200
local C={bg='#151217',text='#E9DCCB',muted='#BCAEAE',gold='#D7AE91',red='#D76C82',rule='#59414A'}
local function seconds(value)
  if type(value)=='table' then return (tonumber(value.secs) or 0)+(tonumber(value.nanos) or 0)/1000000000 end
  return tonumber(value) or 0
end
local function poke(event,payload)
  local argv={'noctalia','msg','plugin','q/tmus:watch','all',event}
  if payload then table.insert(argv,payload) end
  noctalia.runAsync(argv)
end
local function library()
  panel.close()
  noctalia.togglePanel('q/tmus:library')
end
local function icon(key,glyph,tooltip,fn,props)
  props=props or {}
  props.noHints=true
  local node=visual.icon(key,glyph,tooltip,fn,props)
  node.props.x,node.props.y=props.x,props.y
  return node
end
local function draw()
  if not opened then return end
  local s=snapshot
  local present=s.linked==true and tostring(s.trackId or '')~=''
  local liked=type(s.ratings)=='table' and s.ratings[s.trackId]=='liked'
  local position=seconds(s.position)
  if s.status=='playing' then position+=math.max(0,os.time()-(tonumber(s.at) or os.time())) end
  local duration=seconds(s.duration)
  local nodes={
    ui.box({key='compact-field',x=9,y=12,width=W-18,height=H-24,fill=C.bg,border='#9F7863',borderWidth=1,radius=10}),
    ui.box({key='compact-inner',x=14,y=17,width=W-28,height=H-34,fill='#00000000',border=C.rule,borderWidth=1,radius=7}),
    ui.row({key='compact-heading',x=26,y=30,width=W-52,height=65,gap=12,align='center'},{
      visual.art('compact-art',s.artPath,62),
      ui.column({flexGrow=1,gap=5},{
        visual.label('compact-title',present and s.title or s.linked and 'Нет текущего трека' or 'tmusd недоступен',{fontFamily='Noto Serif',fontSize=15}),
        visual.label('compact-artist',present and s.artist or 'Откройте библиотеку',{fontSize=10,color=C.muted}),
      }),
      icon('compact-like','heart',liked and 'Убрать лайк' or 'Нравится',function() poke('rate',(liked and 'none:' or 'like:')..s.trackId) end,{width=26,enabled=present,active=liked}),
    }),
    ui.row({key='compact-controls',x=83,y=105,width=194,height=38,gap=12,align='center',justify='center'},{
      icon('compact-prev','player-skip-back','Предыдущий трек',function() poke('prev') end,{enabled=s.linked}),
      icon('compact-toggle',s.status=='playing' and 'player-pause' or 'player-play',s.status=='playing' and 'Пауза' or 'Слушать',function() poke('toggle') end,{width=38,height=38,radius=19,active=true,enabled=s.linked}),
      icon('compact-next','player-skip-forward','Следующий трек',function() poke('next') end,{enabled=s.linked}),
    }),
    icon('compact-library','music','Библиотека',library,{width=26,height=26,x=26,y=112}),
    ui.progress({key='compact-progress',x=26,y=158,width=W-52,height=3,progress=duration>0 and math.clamp(position/duration,0,1) or 0,fill=C.red,track=C.rule,radius=1}),
    icon('compact-close','x','Закрыть',function() panel.close() end,{width=22,height=22,x=W-46,y=112}),
  }
  for _,corner in ipairs({{'tl',8,10},{'tr',W-39,10},{'bl',8,H-50},{'br',W-39,H-50}}) do
    nodes[#nodes+1]=ui.image({key='compact-corner-'..corner[1],path='skin-assets/mina/corner-'..corner[1]..'.svg',x=corner[2],y=corner[3],width=32,height=42,fit='stretch'})
  end
  panel.render(ui.box({key='mina-compact',width=W,height=H,fill='#00000000'},nodes))
end
local function refresh(value)
  snapshot=type(value)=='table' and value or {}
  draw()
end
function onOpen()
  opened=true
  panel.setWantsSecondTicks(true)
  refresh(noctalia.state.get('tmus'))
end
function onClose() opened=false end
function update() if snapshot.status=='playing' then draw() end end
noctalia.state.watch('tmus',refresh)
noctalia.state.watch('tmus.now',function(value)
  if type(value)~='table' then return end
  for _,key in ipairs({'linked','status','position','duration','volume','at'}) do snapshot[key]=value[key] end
  draw()
end)
