package com.hachiwii.oink

import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.onRoot
import com.github.takahirom.roborazzi.captureRoboImage
import com.hachiwii.oink.ui.*
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class SharedClipboardUiTest {
    @get:Rule val compose = createComposeRule()
    private val pc=PeerUi("pc","Computer","windows")
    private val phone=PeerUi("phone","Phone","android")
    @Test fun activeTargetFollowsFocusAndManualChoiceFallsBackToActive() {
        val targets=TransferTargetsUi(listOf(phone,pc),2,activeDevice="pc")
        assertEquals(pc,composerTarget(targets,targets.online,null).target)
        assertEquals(phone,composerTarget(targets,targets.online,"phone").target)
        val gone=targets.copy(online=listOf(pc))
        assertEquals(pc,composerTarget(gone,listOf(phone,pc),"phone").target)
        assertEquals(phone,composerTarget(targets.copy(activeDevice="phone"),targets.online,null).target)
        assertNull(composerTarget(TransferTargetsUi(emptyList(),2,searching=true),listOf(phone,pc),"phone").switchedFrom)
    }
    @Test fun shareRoutingHonorsSizePhoneOnlyAndTheOfflineSingleMemberException() {
        val targets=TransferTargetsUi(listOf(phone,pc),2,activeDevice="pc")
        assertEquals(pc,automaticShareTarget(targets,299_999_999))
        assertNull(automaticShareTarget(targets,300_000_000))
        assertNull(automaticShareTarget(targets,null))
        assertNull(automaticShareTarget(targets.copy(online=listOf(phone)),1))
        val offline=phone.copy(offline=true)
        assertEquals(offline,automaticShareTarget(TransferTargetsUi(emptyList(),1,only=offline),null))
    }
    @Test fun everyCompletedDirectBubbleKeepsItsRecipientAndPcMessagesHaveNoSentCaption() {
        fun file(id:Long,mine:Boolean,direct:Boolean,to:String?)=MessageUi(id,mine,if(mine)"self" else "phone",if(mine)"This phone" else "Phone","android",true,"file",name="File$id.pdf",progress=100,direct=direct,toName=to)
        val list=listOf(file(1,true,true,"Phone"),file(2,true,true,"Phone"),file(3,false,true,"This phone"),file(4,true,false,null),MessageUi(5,true,"self","This phone","android",true,"text",text="hello",direct=true,toName="Phone"))
        compose.setContent { OinkApp(AppModel(peers=listOf(phone,pc),messages=list),UiInit(tab=Tab.Transfer),Prefs(scheme="light"),{},AppActions(),false) }
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/sc-mobile/direct-captions.png")
        compose.onAllNodesWithText("已发送给 Phone").assertCountEquals(3)
        compose.onAllNodesWithText("已发送给 This phone").assertCountEquals(1)
        compose.onAllNodesWithText("已发送").assertCountEquals(0)
    }
    @Test fun holderOfflineAndExpiredCardsUseTheExistingUnavailableStates() {
        fun file(id:Long,availability:String?)=MessageUi(id,false,"pc","Computer","windows",false,"file",name="File$id.pdf",detail="PDF · 1 MB",unavailable=availability)
        compose.setContent { OinkApp(AppModel(peers=listOf(phone,pc),messages=listOf(file(1,"offline"),file(2,"expired"),file(3,null))),UiInit(tab=Tab.Transfer),Prefs(scheme="dark"),{},AppActions(),false) }
        compose.onAllNodesWithText("暂时无法加载", substring = true).assertCountEquals(1)
        compose.onAllNodesWithText("已失效", substring = true).assertCountEquals(1)
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/sc-mobile/availability-dark.png")
    }
    @Test fun purgingAnOpenBodyClosesItsDetachedViewer() {
        val file = java.io.File.createTempFile("clipboard-body", ".jpg")
        val bitmap = android.graphics.Bitmap.createBitmap(16, 16, android.graphics.Bitmap.Config.ARGB_8888)
        file.outputStream().use { bitmap.compress(android.graphics.Bitmap.CompressFormat.JPEG, 90, it) }
        val image = MessageUi(42,false,"pc","Computer","windows",true,"image",name="Purged.jpg",imagePath=file.path,localPath=file.path,mime="image/jpeg",progress=100)
        val messages = androidx.compose.runtime.mutableStateOf(listOf(image))
        try {
            compose.setContent { OinkApp(AppModel(peers=listOf(pc),messages=messages.value),UiInit(tab=Tab.Transfer,viewer=image),Prefs(scheme="light"),{},AppActions(),false) }
            compose.onNodeWithContentDescription("Purged.jpg").assertExists()
            compose.runOnIdle { file.delete(); messages.value=emptyList() }
            compose.onNodeWithContentDescription("Purged.jpg").assertDoesNotExist()
        } finally { file.delete(); bitmap.recycle() }
    }

    @Test fun retryAppearsOnlyAfterAutoLoadFailed() {
        var retried=0
        compose.setContent { OinkApp(AppModel(peers=listOf(phone,pc),olderLoadFailed=true),UiInit(tab=Tab.Transfer),Prefs(scheme="light"),{},AppActions(loadOlder={retried++}),false) }
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/sc-mobile/history-retry.png")
        compose.onNodeWithText("加载更早的记录").performClick()
        assertEquals(1,retried)
    }
}
